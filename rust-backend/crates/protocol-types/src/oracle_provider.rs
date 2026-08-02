//! Which oracle provider the stack is currently running on (SO-335).
//!
//! This is the type behind *the* switch: `oracle-service` carries one
//! `[oracle] provider` field, serves it on `/oracle/descriptor`, and
//! every other component — Rust composer, browser composer, catalog
//! lookups — derives its behaviour from that one value rather than
//! naming a provider itself.
//!
//! Deliberately NOT `#[non_exhaustive]`: adding a provider should force
//! a compile error at every match site, because each one is a real
//! decision (which feed key, which PTB legs, which package id) and a
//! silent `_ => ` default would be a bug.

use std::fmt;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OracleProvider {
    Pyth,
    Switchboard,
}

impl OracleProvider {
    /// Wire/config spelling. Matches the serde representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            OracleProvider::Pyth => "pyth",
            OracleProvider::Switchboard => "switchboard",
        }
    }

    /// The Move module a provider's adapter package exposes `attest` on.
    /// Package ids are per-deployment and come from token-info; only the
    /// module name is fixed by the source tree.
    pub fn adapter_module(&self) -> &'static str {
        match self {
            OracleProvider::Pyth => "oracle_pyth",
            OracleProvider::Switchboard => "oracle_switchboard",
        }
    }

    pub const ALL: [OracleProvider; 2] = [OracleProvider::Pyth, OracleProvider::Switchboard];
}

impl Default for OracleProvider {
    /// Pyth, because it is the provider that predates the switch — a
    /// deployment whose config says nothing keeps behaving as it did.
    fn default() -> Self {
        OracleProvider::Pyth
    }
}

impl fmt::Display for OracleProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OracleProvider {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pyth" => Ok(OracleProvider::Pyth),
            "switchboard" => Ok(OracleProvider::Switchboard),
            other => Err(anyhow!(
                "unknown oracle provider {other:?} (expected one of: pyth, switchboard)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_str() {
        for p in OracleProvider::ALL {
            assert_eq!(OracleProvider::from_str(p.as_str()).unwrap(), p);
        }
    }

    #[test]
    fn parsing_is_case_and_space_insensitive() {
        assert_eq!(
            OracleProvider::from_str("  SwitchBoard ").unwrap(),
            OracleProvider::Switchboard
        );
    }

    #[test]
    fn unknown_provider_is_rejected_with_the_valid_set() {
        let err = OracleProvider::from_str("chainlink").unwrap_err().to_string();
        assert!(err.contains("chainlink"), "{err}");
        assert!(err.contains("pyth") && err.contains("switchboard"), "{err}");
    }

    #[test]
    fn serde_matches_the_config_spelling() {
        // The config file and the /oracle/descriptor payload must agree,
        // so serde and `as_str` cannot drift apart.
        for p in OracleProvider::ALL {
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(json, format!("\"{}\"", p.as_str()));
            assert_eq!(serde_json::from_str::<OracleProvider>(&json).unwrap(), p);
        }
    }

    #[test]
    fn default_is_pyth() {
        assert_eq!(OracleProvider::default(), OracleProvider::Pyth);
    }
}
