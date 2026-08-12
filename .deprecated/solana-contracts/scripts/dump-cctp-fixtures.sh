#!/usr/bin/env bash
# Dumps Circle CCTP v1 devnet programs + state accounts into
# programs/cctp_bridge/tests/fixtures/ for the LiteSVM tests.
#
# Addresses come from:
#   cargo test -p cctp_bridge print_fixture_addresses -- --ignored --nocapture
set -euo pipefail

cd "$(dirname "$0")/.."
FIXTURES=programs/cctp_bridge/tests/fixtures
mkdir -p "$FIXTURES"

URL=https://api.devnet.solana.com

TMM=CCTPiPYPc6AsJuwueEnWgSgucamXDZwBd53dQ11YiKX3
MT=CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd

solana program dump -u "$URL" "$TMM" "$FIXTURES/token_messenger_minter.so"
solana program dump -u "$URL" "$MT" "$FIXTURES/message_transmitter.so"

dump_account() {
  local name="$1" address="$2"
  solana account -u "$URL" "$address" --output json \
    | python3 -c "
import json, sys
acc = json.load(sys.stdin)
json.dump({'pubkey': '$address', 'account': acc['account'] if 'account' in acc else acc}, sys.stdout, indent=2)
" > "$FIXTURES/$name.json"
  echo "dumped $name -> $FIXTURES/$name.json"
}

ADDRS=$(cargo test -p cctp_bridge print_fixture_addresses -- --ignored --nocapture 2>/dev/null | grep -E '^[a-z_]+ [1-9A-HJ-NP-Za-km-z]{32,44}$')
while read -r name address; do
  dump_account "$name" "$address"
done <<< "$ADDRS"
