#!/usr/bin/env python3
"""Compute the set of services affected by a changed-file list.

Single source of truth for the path -> service mapping consumed by the
GitHub Actions selective-deploy workflow.

`crates/` coverage is DERIVED from the Cargo manifests at runtime — it is
not listed here. `SERVICE_GLOBS` below holds only the paths that no
manifest can tell us about (the service directory, its Dockerfile, and
genuine one-offs). Edit this file when a new service joins the workspace;
a `crates/` boundary shift needs no edit here, and `test_affected.py`
fails if a service's crate coverage ever stops matching its manifest.

Why derived: the crate half of this map used to be hand-maintained and had
silently drifted from the manifests in 23 places across 12 of 17 services
(SO-315). A source-only edit inside an un-watched crate skipped every
service depending on it — the deploy went green while the skipped service
kept running an image built against the old crate.

Usage:
    affected.py <changed-file> [<changed-file> ...]
    # or pipe a newline-separated list on stdin:
    git diff --name-only HEAD^ HEAD | affected.py

Output (stdout): a JSON array of affected service names, sorted.

    $ affected.py rust-backend/services/indexer/src/main.rs
    ["indexer"]

    $ affected.py rust-backend/crates/indexer-graphql/src/lib.rs
    ["api-service","keeper","mm-bot","option-scheduler","price-charting","quoting-service"]

    $ affected.py rust-backend/Cargo.lock
    [... every service ...]   # rebuild_all

Exit code is 0 even when the affected set is empty (e.g. docs-only PR);
the caller short-circuits the workflow on `[]`.

Failure mode is deliberately CLOSED: if a manifest is missing or will not
parse, every service is returned rather than none. Over-deploying costs
build minutes; under-deploying is the bug this file exists to avoid.
"""

from __future__ import annotations

import fnmatch
import json
import sys
import tomllib
from pathlib import Path, PurePosixPath
from typing import Iterable

# Repo root, so the derivation works regardless of the caller's cwd.
# `_deploy.yml` runs this from the repo root inside the checked-out tree,
# so the manifests are present.
REPO_ROOT = Path(__file__).resolve().parents[2]

# Order here is the canonical "all services" list. Keep in sync with the
# ALL_SERVICES array in deployment/ec2/deploy.sh — `test_affected.py`
# asserts the two match.
ALL_SERVICES = ["indexer", "quoting-service", "mm-bot", "option-scheduler", "api-service", "token-info", "auth-service", "gas-station", "hedge-signer", "market-sim", "price-charting", "balance-monitor", "keeper", "oracle-service", "cctp-relay", "dakota-service", "twitter-service", "social-bot"]

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

# Per-service path globs that cannot be derived from a Cargo manifest:
# the service's own source directory, its Dockerfile, and genuine one-offs.
# Crate coverage is NOT listed here — see `crate_globs()` below.
#
# Globs use fnmatch semantics: `**` matches any path segments, `*` matches
# any segment minus `/`.
SERVICE_GLOBS: dict[str, list[str]] = {
    "indexer": [
        "rust-backend/services/indexer/**",
        "rust-backend/Dockerfile.indexer",
    ],
    "quoting-service": [
        "rust-backend/services/quoting-service/**",
        "rust-backend/Dockerfile.quoting",
    ],
    "mm-bot": [
        "rust-backend/services/mm-bot/**",
        "rust-backend/Dockerfile.mm-bot",
    ],
    "option-scheduler": [
        "rust-backend/services/option-scheduler/**",
        "rust-backend/Dockerfile.scheduler",
    ],
    "api-service": [
        "rust-backend/services/api-service/**",
        "rust-backend/Dockerfile.api-service",
    ],
    "token-info": [
        "rust-backend/services/token-info/**",
        "rust-backend/Dockerfile.token-info",
    ],
    "auth-service": [
        "rust-backend/services/auth-service/**",
        "rust-backend/Dockerfile.auth-service",
    ],
    "price-charting": [
        "rust-backend/services/price-charting/**",
        "rust-backend/Dockerfile.price-charting",
    ],
    "cctp-relay": [
        "rust-backend/services/cctp-relay/**",
        "rust-backend/Dockerfile.cctp-relay",
    ],
    # Staging-only service. It still appears here so a source change rebuilds
    # its image; what keeps it out of prod is its absence from
    # docker-compose.prod.yml, which deploy.sh filters against.
    "dakota-service": [
        "rust-backend/services/dakota-service/**",
        "rust-backend/Dockerfile.dakota-service",
    ],
    "gas-station": [
        "rust-backend/services/gas-station/**",
        "rust-backend/Dockerfile.gas-station",
    ],
    "hedge-signer": [
        "rust-backend/services/hedge-signer/**",
        "rust-backend/Dockerfile.hedge-signer",
    ],
    "market-sim": [
        "rust-backend/services/market-sim/**",
        "rust-backend/Dockerfile.market-sim",
    ],
    "balance-monitor": [
        "rust-backend/services/balance-monitor/**",
        "rust-backend/Dockerfile.balance-monitor",
    ],
    "keeper": [
        "rust-backend/services/keeper/**",
        "rust-backend/Dockerfile.keeper",
    ],
    "twitter-service": [
        "rust-backend/services/twitter-service/**",
        "rust-backend/Dockerfile.twitter-service",
    ],
    "social-bot": [
        "rust-backend/services/social-bot/**",
        "rust-backend/Dockerfile.social-bot",
    ],
    "oracle-service": [
        "rust-backend/services/oracle-service/**",
        "rust-backend/Dockerfile.oracle-service",
    ],
}

# Manifest sections whose deps end up compiled into the service image.
# `[dev-dependencies]` is deliberately absent: it never enters the image,
# so a test-only crate must not trigger a redeploy.
_IMAGE_DEP_SECTIONS = ("dependencies", "build-dependencies")


class ManifestError(Exception):
    """A Cargo manifest is missing, unreadable, or not valid TOML."""


def _load_manifest(path: Path) -> dict:
    try:
        with open(path, "rb") as fh:
            return tomllib.load(fh)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise ManifestError(f"{path}: {exc}") from exc


def _dep_names(manifest: dict) -> set[str]:
    """Dep names from every image-bearing section, including `[target.*]`."""
    names: set[str] = set()
    for section in _IMAGE_DEP_SECTIONS:
        names.update(manifest.get(section, {}))
    for cfg in manifest.get("target", {}).values():
        for section in _IMAGE_DEP_SECTIONS:
            names.update(cfg.get(section, {}))
    return names


def _workspace_crates(root: Path) -> dict[str, str]:
    """Map workspace dep name -> `crates/` directory name.

    Read from `[workspace.dependencies]` in the workspace root manifest,
    which is what `{ workspace = true }` in a member manifest resolves
    through. Only `path` entries under `crates/` are workspace crates;
    registry deps (tokio, serde, ...) are not our source tree.
    """
    ws = _load_manifest(root / "rust-backend" / "Cargo.toml")
    deps = ws.get("workspace", {}).get("dependencies", {})
    crates: dict[str, str] = {}
    for name, spec in deps.items():
        path = spec.get("path") if isinstance(spec, dict) else None
        if path is None:
            continue
        parts = PurePosixPath(path).parts
        if len(parts) == 2 and parts[0] == "crates":
            crates[name] = parts[1]
    if not crates:
        raise ManifestError("no crates/ entries in [workspace.dependencies]")
    return crates


def _crate_deps(crates: dict[str, str], manifest_path: Path) -> set[str]:
    """Workspace crates a manifest depends on directly, by directory name."""
    names = _dep_names(_load_manifest(manifest_path))
    return {crates[n] for n in names if n in crates}


def crate_globs(root: Path = REPO_ROOT) -> dict[str, list[str]]:
    """Derive each service's `crates/**` globs from the Cargo manifests.

    Resolves workspace-crate deps TRANSITIVELY: a crate pulled in only via
    another crate is covered too, which the old hand-written map missed one
    level down. Raises `ManifestError` if any manifest is missing or
    unparseable — callers fail closed on that.
    """
    crates = _workspace_crates(root)
    crates_dir = root / "rust-backend" / "crates"
    # Every crate's direct deps, parsed once. Reading all of them up front
    # also means a crate declared in [workspace.dependencies] whose manifest
    # is missing raises here, rather than only when some service reaches it.
    direct = {
        crate: _crate_deps(crates, crates_dir / crate / "Cargo.toml")
        for crate in crates.values()
    }

    derived: dict[str, list[str]] = {}
    for svc in SERVICE_GLOBS:
        seen: set[str] = set()
        queue = list(
            _crate_deps(
                crates, root / "rust-backend" / "services" / svc / "Cargo.toml"
            )
        )
        while queue:
            crate = queue.pop()
            if crate in seen:
                continue
            seen.add(crate)
            queue.extend(direct[crate] - seen)
        derived[svc] = [f"rust-backend/crates/{c}/**" for c in sorted(seen)]
    return derived


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


def service_globs(root: Path = REPO_ROOT) -> dict[str, list[str]]:
    """`SERVICE_GLOBS` plus the crate globs derived from the manifests."""
    derived = crate_globs(root)
    return {svc: globs + derived[svc] for svc, globs in SERVICE_GLOBS.items()}


def affected_services(
    changed_files: Iterable[str], root: Path = REPO_ROOT
) -> list[str]:
    """Return the sorted list of services touched by `changed_files`.

    If any change matches `REBUILD_ALL_GLOBS`, returns the full list.
    Otherwise the union of services whose globs match — `SERVICE_GLOBS`
    plus the crate globs derived from the Cargo manifests.

    Fails CLOSED: if the derivation cannot read the manifests, every
    service is returned. An empty input list still yields `[]`, which is
    the "nothing changed" signal the workflow short-circuits on.
    """
    files = [f for f in (l.strip() for l in changed_files) if f]
    if not files:
        return []

    for f in files:
        if _match_any(f, REBUILD_ALL_GLOBS):
            return sorted(ALL_SERVICES)

    try:
        globs_by_service = service_globs(root)
    except ManifestError as exc:
        print(f"affected.py: {exc}; failing closed to all services", file=sys.stderr)
        return sorted(ALL_SERVICES)

    hit: set[str] = set()
    for svc, globs in globs_by_service.items():
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
