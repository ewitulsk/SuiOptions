#!/usr/bin/env python3
"""Fail if a data-room crate reaches into the protocol side of the workspace.

data-room shares the rust-backend Cargo workspace (SO-449) so the backtester
can reuse the lake crates, but the runtime isolation the data-room spec
(§3, §10) relies on runs the other way too: collectors and batch jobs must
never link a protocol service client, a Sui crate, or the RDS/diesel stack.
A dependency like that would drag the Sui git tree into the collector image
build and couple the lake's deploy cadence to the protocol's.

Rule, checked from the manifests alone (no cargo, no network):

  every dependency of a data-room member — [dependencies],
  [dev-dependencies], [build-dependencies], and [target.*] sections —
  must be either another data-room crate or a crates.io package that is
  not on the deny list below.

A `{ workspace = true }` dependency is resolved through the root
`[workspace.dependencies]` entry, so a protocol crate (`path = "crates/..."`)
or a git dependency (the Sui pins) inherited that way is caught as well.

Usage:
    check_isolation.py [--root <repo root>]      # exit 1 on any violation
"""

from __future__ import annotations

import argparse
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
DATA_ROOM_PREFIX = "data-room/"

# Registry packages that mean "database / protocol access" regardless of how
# they are declared. Prefix matches (`sui-`) cover the whole Sui family.
DENY_NAMES = {
    "diesel",
    "diesel_migrations",
    "diesel-async",
    "r2d2",
    "pq-sys",
    "postgres",
    "tokio-postgres",
    "sqlx",
}
DENY_PREFIXES = ("sui-", "sui_", "move-", "fastcrypto", "shared-crypto")

_DEP_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


def _deps(manifest: dict) -> dict[str, object]:
    out: dict[str, object] = {}
    for section in _DEP_SECTIONS:
        out.update(manifest.get(section, {}))
    for cfg in manifest.get("target", {}).values():
        for section in _DEP_SECTIONS:
            out.update(cfg.get(section, {}))
    return out


def _package_name(name: str, spec: object) -> str:
    """The real package name behind a dependency key (`package = "..."`)."""
    if isinstance(spec, dict) and "package" in spec:
        return str(spec["package"])
    return name


def _denied_name(pkg: str) -> bool:
    return pkg in DENY_NAMES or pkg.startswith(DENY_PREFIXES)


def violations(root: Path) -> list[str]:
    ws_path = root / "rust-backend" / "Cargo.toml"
    ws = tomllib.loads(ws_path.read_text())
    members = ws["workspace"]["members"]
    ws_deps = ws["workspace"].get("dependencies", {})

    data_room_members = [m for m in members if m.startswith(DATA_ROOM_PREFIX)]
    if not data_room_members:
        return [f"{ws_path}: no data-room/ members in [workspace.members]"]

    problems: list[str] = []
    for member in data_room_members:
        manifest_path = root / "rust-backend" / member / "Cargo.toml"
        manifest = tomllib.loads(manifest_path.read_text())
        crate = manifest["package"]["name"]

        for key, spec in _deps(manifest).items():
            # Resolve the effective spec: inherited entries come from the
            # workspace table, local ones are what the member wrote.
            if isinstance(spec, dict) and spec.get("workspace") is True:
                effective = ws_deps.get(key)
                if effective is None:
                    problems.append(f"{crate}: `{key}` inherits a workspace dep that does not exist")
                    continue
            else:
                effective = spec

            pkg = _package_name(key, effective)
            if _denied_name(pkg):
                problems.append(f"{crate}: depends on `{pkg}` (denied: protocol/Sui/RDS)")
                continue
            if isinstance(effective, dict):
                if "git" in effective:
                    problems.append(f"{crate}: depends on git dependency `{pkg}` ({effective['git']})")
                    continue
                path = effective.get("path")
                if path is not None:
                    # Member-local paths are relative to the member dir;
                    # workspace-table paths are relative to the workspace root.
                    base = ws_path.parent if effective is ws_deps.get(key) else manifest_path.parent
                    rel = (base / path).resolve().relative_to((root / "rust-backend").resolve())
                    if not rel.as_posix().startswith(DATA_ROOM_PREFIX):
                        problems.append(f"{crate}: depends on workspace crate `{pkg}` outside data-room/ ({rel.as_posix()})")
    return problems


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", type=Path, default=REPO_ROOT, help="repo root (default: derived from this file)")
    args = ap.parse_args()

    problems = violations(args.root)
    if problems:
        print("data-room isolation check FAILED:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1
    print("data-room isolation check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
