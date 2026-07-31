#!/usr/bin/env bash
# Refuse a destructive operation unless the target env is PROVABLY testnet.
#
# This replaces two guards that could never fire (SO-324 follow-up):
#
#   redeploy-contract.yml  tested $NETWORK == "mainnet", where $NETWORK came
#                          from a `case` hardcoding "testnet" for both envs
#   wipe-provision-db.yml  tested the env name against "production"/"mainnet",
#                          neither of which its `type: choice` input offers
#
# Both were correct today and structurally incapable of firing tomorrow.
#
# The inversion matters more than the wiring. The old guards asked "is this
# mainnet?" and proceeded on anything else — including "I have no idea". This
# asks "can I prove this is testnet?" and refuses otherwise, so an
# unresolvable, ambiguous, half-migrated or unknown environment all land on
# refuse rather than on proceed.
#
# One implementation, called by both workflows. A guard that lives in two
# YAML files only works while someone remembers to edit both, which is the
# same class of defect it is here to remove.
#
#   usage:  assert_testnet.sh <env>
#   stdout: the resolved network, when it is testnet (callers may reuse it)
#   exit 1: refuse — reason on stderr
set -euo pipefail

ENV="${1:?usage: assert_testnet.sh <env>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! NETWORK="$(python3 "$HERE/resolve_network.py" "$ENV" 2>&1)"; then
  echo "REFUSING: cannot establish the network for env '$ENV'." >&2
  echo "  $NETWORK" >&2
  echo "  A destructive operation does not proceed on an environment whose" >&2
  echo "  network is unknown. Resolve the ambiguity rather than bypassing." >&2
  exit 1
fi

if [ "$NETWORK" != "testnet" ]; then
  echo "REFUSING: env '$ENV' resolves to network '$NETWORK'." >&2
  echo "  This workflow publishes packages and/or wipes databases. It is" >&2
  echo "  only ever safe against testnet. If '$ENV' is meant to carry real" >&2
  echo "  funds, this refusal is the intended behaviour." >&2
  exit 1
fi

echo "$NETWORK"
