//! TOML config loading with `${VAR}` env-var expansion.
//!
//! Reads the file, substitutes `${NAME}` from the process env, then hands
//! the resulting string to the `config` crate. Used by the indexer so its
//! `database_url` can pick up `${DB_PASSWORD}` injected at deploy time.

use std::path::Path;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;

/// Read a TOML file from disk, expand `${VAR}` references against
/// `std::env::var`, and deserialize into `T`. Missing env vars are an
/// error — fail fast at boot rather than silently leaving a literal
/// `${VAR}` in a connection string.
pub fn load_toml<T: DeserializeOwned, P: AsRef<Path>>(path: P) -> Result<T> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let expanded = expand_env(&raw)
        .with_context(|| format!("expanding env vars in {}", path.display()))?;
    let settings = config::Config::builder()
        .add_source(config::File::from_str(&expanded, config::FileFormat::Toml))
        .build()
        .with_context(|| format!("loading config {}", path.display()))?;
    settings
        .try_deserialize()
        .with_context(|| format!("parsing config {}", path.display()))
}

/// Substitute `${NAME}` with the value of the env var `NAME`. Treats a
/// missing var as an error (vs. an empty string) so a missed deploy-time
/// secret can't silently produce a bad config.
fn expand_env(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .with_context(|| format!("unterminated ${{ at offset {start}"))?;
        let name = &after[..end];
        let value = std::env::var(name)
            .with_context(|| format!("env var ${{{name}}} not set"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_no_vars() {
        assert_eq!(expand_env("hello").unwrap(), "hello");
    }

    #[test]
    fn expand_one_var() {
        std::env::set_var("CFG_TEST_FOO", "bar");
        assert_eq!(expand_env("x${CFG_TEST_FOO}y").unwrap(), "xbary");
    }

    #[test]
    fn expand_missing_errors() {
        std::env::remove_var("CFG_TEST_MISSING");
        assert!(expand_env("x${CFG_TEST_MISSING}y").is_err());
    }
}
