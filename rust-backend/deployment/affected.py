#!/usr/bin/env python3
"""Compute the set of services affected by a changed-file list.

Single source of truth for the path -> service mapping consumed by the
GitHub Actions selective-deploy workflow. Audit / edit this file when a
new service joins the workspace or a `crates/` boundary shifts.

Usage:
    affected.py <changed-file> [<changed-file> ...]
    # or pipe a newline-separated list on stdin:
    git diff --name-only HEAD^ HEAD | affected.py

Output (stdout): a JSON array of affected service names, sorted.

    $ affected.py rust-backend/services/indexer/src/main.rs
    ["indexer"]

    $ affected.py rust-backend/crates/protocol-types/src/quote.rs
    ["indexer","mm-bot","option-scheduler","quoting-service"]

    $ affected.py rust-backend/Cargo.lock
    ["indexer","mm-bot","option-scheduler","quoting-service"]   # rebuild_all

Exit code is 0 even when the affected set is empty (e.g. docs-only PR);
the caller short-circuits the workflow on `[]`.
"""

from __future__ import annotations

import fnmatch
import json
import sys
from typing import Iterable

# Order here is the canonical "all services" list. Keep in sync with the
# ALL_SERVICES array in deployment/ec2/deploy.sh.
ALL_SERVICES = ["indexer", "quoting-service", "mm-bot", "option-scheduler", "api-service", "token-info", "auth-service", "gas-station", "price-charting", "balance-monitor", "keeper", "oracle-service", "solana-indexer", "solana-token-info", "solana-auth-service", "solana-api-service", "solana-quoting-service", "solana-oracle-service", "solana-price-charting", "solana-gas-station", "solana-keeper", "solana-option-scheduler", "solana-mm-bot", "solana-balance-monitor"]

# Path globs that, when matched, force every service to rebuild +
# redeploy. Catches lockfile churn, workspace-wide config, infra-side
# scripts that all services depend on, and changes to the affected.py
# logic itself (so the workflow re-validates with the new rules).
#
# deployments.json is deliberately NOT here: it's no longer baked into any
# image (token-info reads it from a host bind-mount, shipped by the deploy
# bundle), so a change to it rebuilds nothing. It only ever changes via the
# redeploy-contract workflow, which deploys explicitly.
REBUILD_ALL_GLOBS = [
    "rust-backend/Cargo.lock",
    "rust-backend/Cargo.toml",
    "rust-backend/deployment/**",
    "rust-backend/infra/**",
    ".github/workflows/**",
]

# Per-service path globs. A match means "rebuild + redeploy this
# service". Globs use fnmatch semantics: `**` matches any path segments,
# `*` matches any segment minus `/`.
#
# Crate dependency map (must mirror each service's Cargo.toml):
#   indexer          : protocol-types, runtime-config, cli-spec, deployments
#   quoting-service  : protocol-types, runtime-config, cli-spec, indexer-graphql
#   mm-bot           : protocol-types, runtime-config, cli-spec, sui-tx,
#                      pyth-client, pricing, deployments, api-service-client
#   option-scheduler : protocol-types, runtime-config, cli-spec, sui-tx,
#                      pyth-client, deployments, indexer-graphql
#   api-service      : protocol-types, runtime-config, cli-spec, indexer-graphql,
#                      deployments
#   token-info       : runtime-config, cli-spec, deployments, token-info-client,
#                      auth-client
#   auth-service     : runtime-config, cli-spec, protocol-types
#   gas-station      : runtime-config, cli-spec, sui-tx
#   balance-monitor  : runtime-config, cli-spec, sui-tx, observability
#   keeper           : protocol-types, runtime-config, cli-spec, sui-tx,
#                      pyth-client, pricing, token-info-client,
#                      indexer-graphql, observability
#   solana-indexer   : runtime-config, observability (standalone workspace;
#                      own Cargo.lock inside its service dir)
#   solana-token-info      : runtime-config, cli-spec, solana-deployments,
#                            solana-token-info-client, auth-client
#   solana-auth-service    : runtime-config, cli-spec
#   solana-api-service     : runtime-config, cli-spec, solana-indexer-graphql,
#                            solana-token-info-client, pricing
#   solana-quoting-service : protocol-types, runtime-config, cli-spec,
#                            solana-indexer-graphql, solana-token-info-client
#   solana-oracle-service  : wraps services/oracle-service (path dep!) +
#                            solana-token-info-client — inherits the Sui
#                            oracle-service's crate deps
#   solana-price-charting  : runtime-config, cli-spec, solana-token-info-client,
#                            solana-indexer-graphql, oracle-client, pricing
#   The standalone Solana workspaces (solana-gas-station / solana-keeper /
#   solana-option-scheduler / solana-mm-bot / solana-balance-monitor) each
#   carry their own Cargo.lock in their service dir and path-import
#   crates/solana-tx, which deps the program crates in
#   solana-contracts/programs (and options_math in solana-contracts/crates)
#   — so program changes rebuild them.
#   (every service also depends on observability)
SERVICE_GLOBS: dict[str, list[str]] = {
    "indexer": [
        "rust-backend/services/indexer/**",
        "rust-backend/Dockerfile.indexer",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/deployments/**",
    ],
    "quoting-service": [
        "rust-backend/services/quoting-service/**",
        "rust-backend/Dockerfile.quoting",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/indexer-graphql/**",
    ],
    "mm-bot": [
        "rust-backend/services/mm-bot/**",
        "rust-backend/Dockerfile.mm-bot",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/deployments/**",
        "rust-backend/crates/api-service-client/**",
    ],
    "option-scheduler": [
        "rust-backend/services/option-scheduler/**",
        "rust-backend/Dockerfile.scheduler",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/deployments/**",
        "rust-backend/crates/indexer-graphql/**",
    ],
    "api-service": [
        "rust-backend/services/api-service/**",
        "rust-backend/Dockerfile.api-service",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/indexer-graphql/**",
        "rust-backend/crates/deployments/**",
    ],
    "token-info": [
        "rust-backend/services/token-info/**",
        "rust-backend/Dockerfile.token-info",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/deployments/**",
        "rust-backend/crates/token-info-client/**",
        "rust-backend/crates/auth-client/**",
    ],
    "auth-service": [
        "rust-backend/services/auth-service/**",
        "rust-backend/Dockerfile.auth-service",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/protocol-types/**",
    ],
    "price-charting": [
        "rust-backend/services/price-charting/**",
        "rust-backend/Dockerfile.price-charting",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/api-service-client/**",
        "rust-backend/crates/token-info-client/**",
        # Vault-APY sampler deps (folded in from derived-metric-worker).
        "rust-backend/crates/indexer-graphql/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/protocol-types/**",
    ],
    "gas-station": [
        "rust-backend/services/gas-station/**",
        "rust-backend/Dockerfile.gas-station",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
    ],
    "balance-monitor": [
        "rust-backend/services/balance-monitor/**",
        "rust-backend/Dockerfile.balance-monitor",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
    ],
    "keeper": [
        "rust-backend/services/keeper/**",
        "rust-backend/Dockerfile.keeper",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
        # keeper keeps a direct Hermes path for the on-chain VAA (pyth-client),
        # and reads spot/σ from oracle-service (oracle-client).
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/token-info-client/**",
        "rust-backend/crates/indexer-graphql/**",
    ],
    "solana-indexer": [
        "rust-backend/services/solana-indexer/**",
        "rust-backend/Dockerfile.solana-indexer",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
    ],
    "oracle-service": [
        "rust-backend/services/oracle-service/**",
        "rust-backend/Dockerfile.oracle-service",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/token-info-client/**",
    ],
    "solana-token-info": [
        "rust-backend/services/solana-token-info/**",
        "rust-backend/Dockerfile.solana-token-info",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/solana-deployments/**",
        "rust-backend/crates/solana-token-info-client/**",
        "rust-backend/crates/auth-client/**",
    ],
    "solana-auth-service": [
        "rust-backend/services/solana-auth-service/**",
        "rust-backend/Dockerfile.solana-auth-service",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
    ],
    "solana-api-service": [
        "rust-backend/services/solana-api-service/**",
        "rust-backend/Dockerfile.solana-api-service",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/solana-indexer-graphql/**",
        "rust-backend/crates/solana-token-info-client/**",
        "rust-backend/crates/pricing/**",
    ],
    "solana-quoting-service": [
        "rust-backend/services/solana-quoting-service/**",
        "rust-backend/Dockerfile.solana-quoting-service",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/solana-indexer-graphql/**",
        "rust-backend/crates/solana-token-info-client/**",
    ],
    # Wraps the Sui oracle-service crate (path dep), so it rebuilds on that
    # service's source and crate deps too.
    "solana-oracle-service": [
        "rust-backend/services/solana-oracle-service/**",
        "rust-backend/Dockerfile.solana-oracle-service",
        "rust-backend/services/oracle-service/**",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/token-info-client/**",
        "rust-backend/crates/solana-token-info-client/**",
    ],
    "solana-price-charting": [
        "rust-backend/services/solana-price-charting/**",
        "rust-backend/Dockerfile.solana-price-charting",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/solana-token-info-client/**",
        "rust-backend/crates/solana-indexer-graphql/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/pricing/**",
    ],
    # Standalone Solana workspaces: own dir (incl. their Cargo.lock) +
    # path-imported shared crates + solana-tx and the on-chain program
    # crates it pins (solana-contracts/** paths are repo-relative, like
    # everything else here).
    "solana-gas-station": [
        "rust-backend/services/solana-gas-station/**",
        "rust-backend/Dockerfile.solana-gas-station",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/solana-tx/**",
        "rust-backend/crates/solana-token-info-client/**",
        "solana-contracts/programs/**",
    ],
    "solana-keeper": [
        "rust-backend/services/solana-keeper/**",
        "rust-backend/Dockerfile.solana-keeper",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/solana-indexer-graphql/**",
        "rust-backend/crates/solana-token-info-client/**",
        "rust-backend/crates/solana-tx/**",
        "solana-contracts/programs/**",
        "solana-contracts/crates/**",
    ],
    "solana-option-scheduler": [
        "rust-backend/services/solana-option-scheduler/**",
        "rust-backend/Dockerfile.solana-option-scheduler",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/solana-token-info-client/**",
        "rust-backend/crates/solana-indexer-graphql/**",
        "rust-backend/crates/solana-tx/**",
        "solana-contracts/programs/**",
        "solana-contracts/crates/**",
    ],
    "solana-mm-bot": [
        "rust-backend/services/solana-mm-bot/**",
        "rust-backend/Dockerfile.solana-mm-bot",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/oracle-client/**",
        "rust-backend/crates/solana-token-info-client/**",
        "rust-backend/crates/solana-indexer-graphql/**",
        "rust-backend/crates/solana-tx/**",
        "solana-contracts/programs/**",
        "solana-contracts/crates/**",
    ],
    "solana-balance-monitor": [
        "rust-backend/services/solana-balance-monitor/**",
        "rust-backend/Dockerfile.solana-balance-monitor",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/observability/**",
        "rust-backend/crates/solana-tx/**",
        "solana-contracts/programs/**",
    ],
}


def _match_any(path: str, globs: Iterable[str]) -> bool:
    """fnmatch with `**` treated as recursive-anything.

    fnmatch already treats `*` as non-slash-aware, which works for our
    `crates/<name>/**` style globs because we want the match to span
    directory boundaries. Translating `**` to `*` is enough.
    """
    for g in globs:
        if fnmatch.fnmatchcase(path, g.replace("**", "*")):
            return True
    return False


def affected_services(changed_files: Iterable[str]) -> list[str]:
    """Return the sorted list of services touched by `changed_files`.

    If any change matches `REBUILD_ALL_GLOBS`, returns the full list.
    Otherwise the union of services whose `SERVICE_GLOBS` match.
    """
    files = [f for f in (l.strip() for l in changed_files) if f]
    if not files:
        return []

    for f in files:
        if _match_any(f, REBUILD_ALL_GLOBS):
            return sorted(ALL_SERVICES)

    hit: set[str] = set()
    for svc, globs in SERVICE_GLOBS.items():
        for f in files:
            if _match_any(f, globs):
                hit.add(svc)
                break
    return sorted(hit)


def _read_inputs(argv: list[str]) -> list[str]:
    if len(argv) > 1:
        return argv[1:]
    return sys.stdin.read().splitlines()


def main() -> int:
    services = affected_services(_read_inputs(sys.argv))
    json.dump(services, sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
