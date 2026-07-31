#!/usr/bin/env python3
"""Resolve which Sui network an environment is actually deployed to.

Why this exists
---------------
Two destructive workflows carried a guard that refused to run against
mainnet, and neither could ever fire:

    redeploy-contract.yml   guarded on $NETWORK, which a `case` statement
                            hardcoded to "testnet" for BOTH envs
    wipe-provision-db.yml   guarded on the env being named "production" or
                            "mainnet", strings its own `type: choice` input
                            cannot produce

Both guards were correct today and structurally incapable of firing. The
defect was not the comparisons — it was that neither guard read anything
that changes when an environment becomes mainnet. You could flip every
service config and both would still pass.

This resolves the network from the service configs, which ARE what change.
Flipping an env to mainnet flips this resolver in the same edit, so the
guard moves with the fact instead of having to be remembered separately.

Contract
--------
    resolve_network.py <env>

    exit 0  -> stdout is exactly one of the known networks
    exit 1  -> stdout is EMPTY, stderr explains why

Callers must treat a non-zero exit as "refuse", not as "assume testnet".
Every failure mode here is deliberately un-guessable: an environment whose
network cannot be established is exactly the case where a destructive
operation must not proceed.
"""

from __future__ import annotations

import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]

# Networks a service config may legitimately declare. Anything else is a
# typo or a new network nobody taught this resolver about; both are
# ambiguity, and ambiguity fails closed.
KNOWN_NETWORKS = frozenset({"mainnet", "testnet", "devnet", "localnet"})

# cctp-relay is deliberately NOT part of its environment's network.
#
# The bridge runs against mainnet USDC even on staging. render-secrets.sh
# says so explicitly: "the relay's [sui].network is config-driven and
# independent of it (mainnet on staging, testnet on prod)". It declares
# `[sui].network` and `[solana].network` rather than a top-level `network`,
# and the Solana one is not a Sui network at all.
#
# Including it would make staging permanently ambiguous and fail every
# guard, so it is excluded — but named here rather than skipped silently,
# because an unexplained exclusion is how a resolver quietly stops covering
# what it claims to.
INDEPENDENT_OF_ENV_NETWORK = frozenset({"cctp-relay"})


class Ambiguous(Exception):
    """The network could not be established. Callers must refuse."""


def _service_configs(env: str, root: Path) -> list[Path]:
    return sorted(root.glob(f"services/*/config/config.{env}.toml"))


def declared_networks(env: str, root: Path = REPO_ROOT) -> dict[str, str]:
    """Map service -> declared top-level network, for this env.

    Raises Ambiguous if a non-excluded service declares its network somewhere
    this function does not read. That check is the point: without it, a
    service that moved its network into a `[sui]` table would be silently
    dropped from the sample and the resolver would keep reporting a
    confident answer over a shrinking set.
    """
    found: dict[str, str] = {}
    for cfg in _service_configs(env, root):
        service = cfg.parts[-3]
        if service in INDEPENDENT_OF_ENV_NETWORK:
            continue
        try:
            with cfg.open("rb") as fh:
                data = tomllib.load(fh)
        except (OSError, tomllib.TOMLDecodeError) as exc:
            raise Ambiguous(f"{cfg}: unreadable ({exc})") from exc

        top = data.get("network")
        nested = {
            f"{table}.network": value.get("network")
            for table, value in data.items()
            if isinstance(value, dict) and "network" in value
        }
        if nested:
            raise Ambiguous(
                f"{service} declares {sorted(nested)} in a table; this resolver "
                f"only reads the top-level `network`. Either move it to the top "
                f"level or add {service} to INDEPENDENT_OF_ENV_NETWORK with a "
                f"reason."
            )
        if top is None:
            continue
        if top not in KNOWN_NETWORKS:
            raise Ambiguous(f"{service} declares unknown network {top!r}")
        found[service] = top
    return found


def resolve(env: str, root: Path = REPO_ROOT) -> str:
    """The env's Sui network, or raise Ambiguous.

    Unanimity is required. A partially-migrated environment — some services
    flipped to mainnet, some not — is the single most likely way this goes
    wrong in practice, and it resolves to a refusal rather than to whichever
    value happens to be in the majority.
    """
    declared = declared_networks(env, root)
    if not declared:
        raise Ambiguous(
            f"no service config declares a network for env {env!r} "
            f"(looked in services/*/config/config.{env}.toml)"
        )
    distinct = sorted(set(declared.values()))
    if len(distinct) > 1:
        detail = ", ".join(f"{s}={n}" for s, n in sorted(declared.items()))
        raise Ambiguous(
            f"env {env!r} declares {len(distinct)} different networks "
            f"({', '.join(distinct)}) — partially migrated? [{detail}]"
        )
    return distinct[0]


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: resolve_network.py <env>", file=sys.stderr)
        return 2
    try:
        print(resolve(argv[1]))
    except Ambiguous as exc:
        print(f"resolve_network: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
