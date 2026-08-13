//! Every publishable Move package must compile with the **deploy
//! compiler** (SO-335).
//!
//! ## Why this exists
//!
//! Publishing does not shell out to the `sui` CLI. It calls
//! `sui_move_build::BuildConfig` — a Rust crate pinned by `Cargo.lock`
//! (`sui-move-build` on the `framework/mainnet` branch) — from
//! `deploy.rs::publish_package_inner`. That crate, at that rev, is the
//! only thing whose opinion actually decides whether a redeploy
//! succeeds.
//!
//! `move-ci.yml` used to approximate this by pinning its CLI to the same
//! version, on the reasoning that "a newer CLI can accept code the deploy
//! compiler rejects". That proxy is strictly weaker than asking the
//! deploy compiler directly, and it produced a false positive: two
//! packages failed the CLI's `sui move test` step (a VM/framework issue
//! at *test* runtime) while building perfectly under the deploy
//! compiler, which never runs Move tests at all.
//!
//! So the proxy is replaced by the real check: this test. If it passes,
//! a redeploy will compile. Run it before any redeploy:
//!
//! ```text
//! cargo test -p deployment-manager --test deploy_build
//! ```
//!
//! It is not in CI because the workspace has no Rust CI job today, and
//! adding one means building the whole Sui dependency tree. Until that
//! changes this is a local/pre-deploy gate.

use std::path::PathBuf;

use sui_move_build::BuildConfig;

/// Every package `deployment-manager` publishes, in dependency order.
///
/// Keep in lockstep with the publish sequence in `main.rs`. A package
/// that ships but is missing here is exactly the gap this test closes.
const PUBLISHABLE: &[&str] = &[
    // `auction` and `rfq` are retired (contracts/.deprecated/) and no
    // longer published, so they are deliberately absent here.
    // The standalone ingress whitelist publishes FIRST: every gated
    // package (core, trading-vault, exchange, exchange-adapter) links
    // against it.
    "whitelist",
    "core",
    "trading-vault",
    "oracle-pyth",
    "oracle-switchboard",
    "deepbook-adapter",
    "options-adapter",
    "equity-oracle",
    "mm-collateral",
    // Hybrid exchange settlement package (standalone audit scope; published
    // by the default protocol pipeline, or alone via --deploy-exchange).
    "exchange",
    "exchange-adapter",
];

fn contracts_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../contracts")
        .canonicalize()
        .expect("locating contracts/ relative to deployment-manager")
}

#[test]
fn deploy_compiler_builds_every_publishable_package() {
    let root = contracts_root();
    let mut failures = Vec::new();

    for name in PUBLISHABLE {
        let path = root.join(name);
        assert!(
            path.join("Move.toml").is_file(),
            "{name} is listed as publishable but has no Move.toml at {}",
            path.display()
        );
        match BuildConfig::new_for_testing().build(&path) {
            Ok(compiled) => {
                let modules = compiled.get_package_bytes(false);
                assert!(
                    !modules.is_empty(),
                    "{name} compiled to zero modules — a publish would send an empty package"
                );
            }
            Err(e) => failures.push(format!("{name}: {e:#}")),
        }
    }

    assert!(
        failures.is_empty(),
        "the deploy compiler rejected {} package(s) — a redeploy would fail:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

/// The publishable list must not drift from what `main.rs` actually
/// publishes. Cheap textual check: every `contracts_root.join("…")` in
/// the publish path should appear above.
#[test]
fn publishable_list_covers_every_package_main_publishes() {
    let main_rs = include_str!("../src/main.rs");
    let mut missing = Vec::new();
    for line in main_rs.lines() {
        let Some(rest) = line.split("contracts_root.join(\"").nth(1) else {
            continue;
        };
        let Some(name) = rest.split('"').next() else {
            continue;
        };
        if !PUBLISHABLE.contains(&name) {
            missing.push(name.to_string());
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "main.rs publishes package(s) this test never builds: {missing:?} — \
         add them to PUBLISHABLE"
    );
}
