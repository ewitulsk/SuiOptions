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
//! # Optional JSON-RPC endpoint override. When set, every binary that builds
//! # a SuiClient through `resolve_rpc_url` uses this instead of the public
//! # network default — lets us point the fleet at a rate-limit-lifted RPC
//! # provider. Absent → public endpoint.
//! rpc_url = "https://1rpc.io/<token>/sui"
//!
//! [mm_bot]
//! # 32-byte secret used to sign Quotes. Interpretation depends on the
//! # MM bot's configured `signing_scheme`: Ed25519 seed, Secp256k1 scalar,
//! # or Secp256r1 scalar.
//! quote_key = "0xabcdef..."
//!
//! [pyth]
//! # API key sent as `Authorization: Bearer …` on every Hermes and
//! # Benchmarks request (the keeper, mm-bot and scheduler all attach it to
//! # their Pyth HTTP client). Lifts the 10-req/10s per-IP rate limit and is
//! # mandatory for Pyth Core access from 2026-07-31.
//! api_key = "..."
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
    #[serde(default)]
    pub auth: AuthSecrets,
    #[serde(default)]
    pub pyth: PythSecrets,
    #[serde(default)]
    pub solana: SolanaSecrets,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SolanaSecrets {
    /// Relayer keypairs (cctp-relay): base58-encoded 64-byte secret or a
    /// JSON byte array (solana-cli id.json format). Keyed per network like
    /// `[sui]`.
    pub devnet: Option<String>,
    pub mainnet: Option<String>,
    /// Used when the per-network slot is unset.
    pub default: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SuiSecrets {
    pub testnet: Option<String>,
    pub mainnet: Option<String>,
    pub devnet: Option<String>,
    /// Used when the per-network slot is unset.
    pub default: Option<String>,
    /// Optional JSON-RPC endpoint override shared by every binary. When set,
    /// `Secrets::resolve_rpc_url` returns this instead of the public network
    /// default. Absent → public endpoint.
    pub rpc_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MmBotSecrets {
    /// 32-byte secret in hex (`0x…` prefix optional). Interpreted per the
    /// MM bot's configured `signing_scheme`.
    pub quote_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AuthSecrets {
    /// HMAC-SHA256 secret the auth-service signs and verifies JWTs with.
    /// auth-service is the only holder; token-info delegates verification
    /// over an internal route and never sees this value.
    pub jwt_secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PythSecrets {
    /// Pyth API key, sent as `Authorization: Bearer …` on Hermes and
    /// Benchmarks requests. Optional: when absent the client falls back to
    /// the (rate-limited) anonymous tier.
    pub api_key: Option<String>,
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
            has_jwt_secret = result.auth.jwt_secret.is_some(),
            has_pyth_api_key = result.pyth.api_key.is_some(),
            has_rpc_url = result.sui.rpc_url.is_some(),
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

    /// Solana keypair for `network` (`mainnet` / `devnet`), falling back to
    /// `solana.default`. Same contract as `sui_private_key`.
    pub fn solana_private_key(&self, network: &str) -> Result<&str> {
        debug!(network, "resolving solana private key");
        let per_net = match network.to_ascii_lowercase().as_str() {
            "mainnet" => &self.solana.mainnet,
            "devnet" => &self.solana.devnet,
            other => return Err(anyhow!("unknown solana network slot: {other}")),
        };
        per_net
            .as_deref()
            .or(self.solana.default.as_deref())
            .ok_or_else(|| {
                anyhow!(
                    "secrets.toml has no solana.{network} key and no solana.default \
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

    pub fn jwt_secret(&self) -> Result<&str> {
        self.auth
            .jwt_secret
            .as_deref()
            .ok_or_else(|| anyhow!("secrets.toml is missing auth.jwt_secret"))
    }

    /// Pyth API key if present. Unlike the signing keys this is optional —
    /// callers attach it as a Bearer header when set and otherwise fall back
    /// to the anonymous (rate-limited) tier.
    pub fn pyth_api_key(&self) -> Option<&str> {
        self.pyth.api_key.as_deref()
    }

    /// JSON-RPC endpoint to build a SuiClient against: the operator-provided
    /// `sui.rpc_url` override if set, else `fallback` (the caller's public
    /// default for its network, e.g. `Network::rpc_url()`). Never errors — a
    /// missing override degrades to the public endpoint so a mis-rendered or
    /// absent secret can't crash-loop a service.
    pub fn resolve_rpc_url(&self, fallback: &str) -> String {
        self.sui
            .rpc_url
            .clone()
            .unwrap_or_else(|| fallback.to_string())
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

[pyth]
api_key = "pyth-test-key"
"#,
        );
        let s = Secrets::load(&p).unwrap();
        assert_eq!(s.sui_private_key("testnet").unwrap(), "suiprivkey1testnet");
        assert_eq!(s.sui_private_key("mainnet").unwrap(), "suiprivkey1mainnet");
        // No devnet entry; falls back to default.
        assert_eq!(s.sui_private_key("devnet").unwrap(), "suiprivkey1default");
        assert_eq!(s.mm_quote_key().unwrap(), "0xdeadbeef");
        assert_eq!(s.pyth_api_key(), Some("pyth-test-key"));
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
    fn resolve_rpc_url_prefers_override_else_fallback() {
        let with = write_tmp(
            "rpc_set",
            "[sui]\ntestnet = \"k\"\nrpc_url = \"https://private.example/sui\"\n",
        );
        let s = Secrets::load(&with).unwrap();
        assert_eq!(
            s.resolve_rpc_url("https://fallback.example"),
            "https://private.example/sui"
        );
        std::fs::remove_file(&with).ok();

        let without = write_tmp("rpc_unset", "[sui]\ntestnet = \"k\"\n");
        let s = Secrets::load(&without).unwrap();
        assert_eq!(
            s.resolve_rpc_url("https://fallback.example"),
            "https://fallback.example"
        );
        std::fs::remove_file(&without).ok();
    }

    #[test]
    fn missing_file_errors() {
        let p = std::path::PathBuf::from("/this/should/not/exist/secrets.toml");
        assert!(Secrets::load(&p).is_err());
    }
}
