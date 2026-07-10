#!/usr/bin/env bash
# Deploy the three programs to devnet in dependency order
# (core → venue → vault), then initialize core's config + treasury.
#
# Prereqs: solana CLI configured with a funded devnet keypair
#   solana config set --url devnet && solana airdrop 5
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER="${CLUSTER:-devnet}"

echo "── building ──"
anchor build

deploy() {
  local name="$1"
  echo "── deploying $name to $CLUSTER ──"
  solana program deploy \
    --url "$CLUSTER" \
    --program-id "target/deploy/${name}-keypair.json" \
    "target/deploy/${name}.so"
}

# Dependency order matches the audit/release ladder: core is standalone,
# the venue CPIs core, the vault CPIs both.
deploy options_core
deploy auction_venue
deploy options_vault

echo "── program ids ──"
for name in options_core auction_venue options_vault; do
  echo "$name: $(solana address -k target/deploy/${name}-keypair.json)"
done

echo
echo "Next steps:"
echo "  1. call options_core::initialize (creates config + treasury PDAs)"
echo "  2. create buckets via options_core::create_bucket / create_put_bucket"
echo "  3. (vault) options_vault::create_vault with pinned Pyth feed ids"
