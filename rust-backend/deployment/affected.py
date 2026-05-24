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
ALL_SERVICES = ["indexer", "quoting-service", "mm-bot", "option-scheduler"]

# Path globs that, when matched, force every service to rebuild +
# redeploy. Catches lockfile churn, workspace-wide config, infra-side
# scripts that all services depend on, and changes to the affected.py
# logic itself (so the workflow re-validates with the new rules).
REBUILD_ALL_GLOBS = [
    "rust-backend/Cargo.lock",
    "rust-backend/Cargo.toml",
    "rust-backend/deployments.json",
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
#   quoting-service  : protocol-types, runtime-config, cli-spec
#   mm-bot           : protocol-types, runtime-config, cli-spec, sui-tx,
#                      pyth-client, pricing, deployments
#   option-scheduler : protocol-types, runtime-config, cli-spec, sui-tx,
#                      pyth-client, deployments
SERVICE_GLOBS: dict[str, list[str]] = {
    "indexer": [
        "rust-backend/services/indexer/**",
        "rust-backend/Dockerfile.indexer",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/deployments/**",
    ],
    "quoting-service": [
        "rust-backend/services/quoting-service/**",
        "rust-backend/Dockerfile.quoting",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/cli-spec/**",
    ],
    "mm-bot": [
        "rust-backend/services/mm-bot/**",
        "rust-backend/Dockerfile.mm-bot",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/pricing/**",
        "rust-backend/crates/deployments/**",
    ],
    "option-scheduler": [
        "rust-backend/services/option-scheduler/**",
        "rust-backend/Dockerfile.scheduler",
        "rust-backend/crates/protocol-types/**",
        "rust-backend/crates/runtime-config/**",
        "rust-backend/crates/cli-spec/**",
        "rust-backend/crates/sui-tx/**",
        "rust-backend/crates/pyth-client/**",
        "rust-backend/crates/deployments/**",
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
