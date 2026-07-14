//! Default program ids come from `solana-contracts/Anchor.toml`
//! (`[programs.localnet]` — the ids are cluster-independent, fixed by the
//! deploy keypairs), so the common path needs no id flags.

use std::path::Path;

use anyhow::{Context, Result};

pub struct ProgramIds {
    pub core: String,
    pub venue: String,
    pub vault: String,
}

pub fn load_program_ids(anchor_toml: &Path) -> Result<ProgramIds> {
    let raw = std::fs::read_to_string(anchor_toml)
        .with_context(|| format!("reading {}", anchor_toml.display()))?;
    let value: toml::Value = raw
        .parse()
        .with_context(|| format!("parsing {}", anchor_toml.display()))?;
    let programs = value
        .get("programs")
        .and_then(|p| p.get("localnet"))
        .with_context(|| format!("{} has no [programs.localnet] table", anchor_toml.display()))?;
    let get = |name: &str| -> Result<String> {
        programs
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .with_context(|| {
                format!(
                    "{} [programs.localnet] has no `{name}` entry",
                    anchor_toml.display()
                )
            })
    };
    Ok(ProgramIds {
        core: get("options_core")?,
        venue: get("auction_venue")?,
        vault: get("options_vault")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_real_anchor_toml() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../solana-contracts/Anchor.toml"
        );
        let ids = load_program_ids(Path::new(path)).unwrap();
        // The declared ids in the program crates are the source of truth;
        // Anchor.toml must agree or deploys would target the wrong ids.
        assert_eq!(ids.core, options_core::ID.to_string());
        assert!(!ids.venue.is_empty());
        assert!(!ids.vault.is_empty());
    }
}
