//! The MM's own collateral deployment (plan §8: "deploy, don't create").
//!
//! Publish machinery lives in `move_publish::collateral` (shared with the
//! deployment-manager's `--deploy-mm-collateral`, which the redeploy-contract
//! workflow runs after every options_core republish). This module keeps the
//! bot-side concerns: where the persisted state file lives and how serve mode
//! resolves the routing.
//!
//! Serve-mode resolution order:
//!   1. explicit `collateral_package`/`collateral_account` in the bot config
//!   2. the deploy-bundle mount `/run/mm-bot/collateral.<network>.toml` —
//!      the redeploy-contract workflow commits the state file and the deploy
//!      bundle drops it on the host (same mechanism as deployments.json), so
//!      a contract redeploy refreshes it WITHOUT rebuilding this image
//!   3. the repo-relative default path (local dev / standalone MMs running
//!      `mm-bot deploy-collateral` from the workspace root)

use std::path::PathBuf;

use anyhow::{Context, Result};
use sui_types::base_types::ObjectID;

pub use move_publish::collateral::{deploy, load, store, CollateralDeployment};

/// Default state-file path for `network`, relative to the workspace root —
/// where `deploy-collateral` writes and local runs read.
pub fn default_state_path(network: &str) -> PathBuf {
    PathBuf::from(format!("services/mm-bot/config/collateral.{network}.toml"))
}

/// Deploy-bundle mount point inside the container (docker-compose mounts the
/// host's `/opt/options/<env>/mm-bot` here, populated by the deploy bundle).
pub fn bundle_state_path(network: &str) -> PathBuf {
    PathBuf::from(format!("/run/mm-bot/collateral.{network}.toml"))
}

/// Resolve the collateral routing for serve mode: explicit config values win,
/// else the first `candidates` path that exists is loaded (a file for the
/// wrong network is an error, not a fall-through — a stale bundle must not be
/// silently masked by a different file).
pub fn resolve(
    config_package: Option<&str>,
    config_account: Option<&str>,
    candidates: &[PathBuf],
    network: &str,
) -> Result<(ObjectID, ObjectID)> {
    if let (Some(pkg), Some(acct)) = (config_package, config_account) {
        return Ok((
            ObjectID::from_hex_literal(pkg).context("parsing collateral_package")?,
            ObjectID::from_hex_literal(acct).context("parsing collateral_account")?,
        ));
    }
    let Some(state_path) = candidates.iter().find(|p| p.exists()) else {
        anyhow::bail!(
            "no collateral deployment: set collateral_package/collateral_account in the bot \
             config, or run `mm-bot deploy-collateral` (looked at {})",
            candidates
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    let dep = load(state_path)?;
    if dep.network != network {
        anyhow::bail!(
            "collateral state {} is for network {} but the bot runs on {network}",
            state_path.display(),
            dep.network
        );
    }
    Ok((
        ObjectID::from_hex_literal(&dep.package_id).context("parsing persisted package_id")?,
        ObjectID::from_hex_literal(&dep.account_id).context("parsing persisted account_id")?,
    ))
}

/// The serve-mode candidate list for `network`, in resolution order.
pub fn state_path_candidates(network: &str) -> Vec<PathBuf> {
    vec![bundle_state_path(network), default_state_path(network)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_round_trips() {
        let dir = std::env::temp_dir().join(format!("mm-collateral-test-{}", std::process::id()));
        let path = dir.join("collateral.staging.toml");
        let dep = CollateralDeployment {
            network: "staging".into(),
            package_id: "0x1111111111111111111111111111111111111111111111111111111111111111".into(),
            account_id: "0x2222222222222222222222222222222222222222222222222222222222222222".into(),
            upgrade_cap: "0x3333333333333333333333333333333333333333333333333333333333333333".into(),
        };
        store(&path, &dep).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.package_id, dep.package_id);
        assert_eq!(back.account_id, dep.account_id);
        let (pkg, acct) = resolve(None, None, &[path.clone()], "staging").unwrap();
        assert_eq!(pkg.to_hex_uncompressed(), dep.package_id);
        assert_eq!(acct.to_hex_uncompressed(), dep.account_id);
        // Wrong network in a FOUND file is refused, not skipped.
        assert!(resolve(None, None, &[path.clone()], "prod").is_err());
        // Missing candidates before the real one are skipped.
        let (pkg, _) = resolve(
            None,
            None,
            &[PathBuf::from("/nonexistent/collateral.toml"), path.clone()],
            "staging",
        )
        .unwrap();
        assert_eq!(pkg.to_hex_uncompressed(), dep.package_id);
        // Explicit config wins without touching any file.
        let (pkg, _) = resolve(
            Some("0xaa"),
            Some("0xbb"),
            &[PathBuf::from("/nonexistent")],
            "staging",
        )
        .unwrap();
        assert_eq!(pkg, ObjectID::from_hex_literal("0xaa").unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}
