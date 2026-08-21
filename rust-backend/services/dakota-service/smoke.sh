#!/usr/bin/env bash
#
# End-to-end smoke test for the Dakota integration.
#
# Exercises the whole story against a running auth-service + dakota-service:
# admin bootstrap, the three-tier customer hierarchy, scope isolation, the
# ramps, sandbox funding and the activity ledger. Every assertion is one a
# regression would actually break.
#
#   AUTH=http://127.0.0.1:9007 AUTHI=http://127.0.0.1:9008 \
#   DK=http://127.0.0.1:9019 ./smoke.sh
#
# Talks to Dakota's SANDBOX through the service, so it creates real sandbox
# objects (customers, accounts). They are cheap and cannot move real money.
#
# Requires: curl, python3.

set -euo pipefail

AUTH="${AUTH:-http://127.0.0.1:9007}"
AUTHI="${AUTHI:-http://127.0.0.1:9008}"
DK="${DK:-http://127.0.0.1:9019}"
RUN="smoke-$(date +%s)"

pass=0; fail=0
ok()   { printf '  \033[32m✓\033[0m %s\n' "$1"; pass=$((pass+1)); }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$1"; fail=$((fail+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (want $3, got $2)"; fi; }
# Print a field, or the raw body on a parse failure. Dakota rate-limits around
# 100 req/min and this script is chatty, so a bare traceback here would look
# like a code bug when it is really a 429.
jq_(){ python3 -c "
import sys,json
raw=sys.stdin.read()
try:
    d=json.loads(raw)
except Exception:
    sys.stderr.write('  !! non-JSON response: '+raw[:200]+'\n'); sys.exit(1)
print($1)"; }
code(){ curl -sS -o /dev/null -w '%{http_code}' "$@"; }

# Dakota rate-limits; a short pause between phases keeps a long run under it.
breathe(){ sleep "${SMOKE_PAUSE:-2}"; }

section(){ printf '\n\033[1m%s\033[0m\n' "$1"; }

section "1. identity"
ADMIN_INV=$(curl -sS -X POST "$AUTHI/invites" -H 'content-type: application/json' \
  -d '{"role":"admin","label":"smoke"}' | jq_ "d['invite_id']")
AT=$(curl -sS -X POST "$AUTH/register" -H 'content-type: application/json' \
  -d "{\"invite\":\"$ADMIN_INV\",\"username\":\"$RUN-admin\",\"password\":\"correct horse battery staple\"}" | jq_ "d['token']")
[ -n "$AT" ] && ok "admin registered from an invite" || bad "admin registration"

check "an invite is single-use" \
  "$(code -X POST "$AUTH/register" -H 'content-type: application/json' \
      -d "{\"invite\":\"$ADMIN_INV\",\"username\":\"$RUN-dupe\",\"password\":\"correct horse battery staple\"}")" 400

ROLE=$(curl -sS -X POST "$AUTH/login/password" -H 'content-type: application/json' \
  -d "{\"username\":\"$RUN-admin\",\"password\":\"correct horse battery staple\"}" | jq_ "d['role']")
check "password login returns the admin role" "$ROLE" admin
check "a wrong password is refused" \
  "$(code -X POST "$AUTH/login/password" -H 'content-type: application/json' \
      -d "{\"username\":\"$RUN-admin\",\"password\":\"wrong\"}")" 401
check "an unknown user is refused identically" \
  "$(code -X POST "$AUTH/login/password" -H 'content-type: application/json' \
      -d '{"username":"nobody-at-all","password":"whatever"}')" 401

AH="authorization: Bearer $AT"

section "2. catalog"
curl -sS -X PUT "$DK/admin/assets" -H "$AH" -H 'content-type: application/json' \
  -d '{"symbol":"USDC","network_id":"base-sepolia","onramp_enabled":true,"offramp_enabled":true,"swap_enabled":true,"sort_order":0}' >/dev/null
ok "asset enabled"
NETS=$(curl -sS "$DK/catalog" -H "$AH" | jq_ "len([n for n in d['networks'] if 'mainnet' in n])")
check "mainnets are filtered out of the offering" "$NETS" 0

breathe
section "3. hierarchy"
BIZ=$(curl -sS -X POST "$DK/customers" -H "$AH" -H 'content-type: application/json' \
  -d "{\"name\":\"$RUN Partner\",\"customer_type\":\"business\",\"external_ref\":\"$RUN-biz\",\"is_sub_client\":true,\"with_invite\":true}")
BIZ_ID=$(echo "$BIZ" | jq_ "d['customer']['dakota_customer_id']")
BIZ_INV=$(echo "$BIZ" | jq_ "d['invite']['invite_id']")
echo "$BIZ" | jq_ "d['application_url']" | grep -q 'platform.sandbox.dakota.xyz/applications' \
  && ok "hosted onboarding url returned (no PII collected by us)" || bad "application_url"

BT=$(curl -sS -X POST "$AUTH/register" -H 'content-type: application/json' \
  -d "{\"invite\":\"$BIZ_INV\",\"username\":\"$RUN-biz\",\"password\":\"another good long passphrase\"}" | jq_ "d['token']")
BH="authorization: Bearer $BT"
check "the business session is scoped to itself" "$(curl -sS "$AUTH/me" -H "$BH" | jq_ "d['scope']")" "$BIZ_ID"

IND=$(curl -sS -X POST "$DK/customers" -H "$BH" -H 'content-type: application/json' \
  -d "{\"name\":\"$RUN Jane\",\"customer_type\":\"individual\",\"external_ref\":\"$RUN-jane\",\"with_invite\":true}")
IND_ID=$(echo "$IND" | jq_ "d['customer']['dakota_customer_id']")
IND_INV=$(echo "$IND" | jq_ "d['invite']['invite_id']")
check "the business's customer is filed beneath it" \
  "$(echo "$IND" | jq_ "d['customer']['sub_client_id']")" "$BIZ_ID"

IT=$(curl -sS -X POST "$AUTH/register" -H 'content-type: application/json' \
  -d "{\"invite\":\"$IND_INV\",\"username\":\"$RUN-jane\",\"password\":\"jane has a long passphrase\"}" | jq_ "d['token']")
IH="authorization: Bearer $IT"

OTHER_ID=$(curl -sS -X POST "$DK/customers" -H "$AH" -H 'content-type: application/json' \
  -d "{\"name\":\"$RUN Outsider\",\"customer_type\":\"individual\",\"external_ref\":\"$RUN-out\"}" | jq_ "d['customer']['dakota_customer_id']")

section "4. isolation"
check "business reads its own customer"        "$(code "$DK/customers/$IND_ID"   -H "$BH")" 200
check "business cannot read an outsider"       "$(code "$DK/customers/$OTHER_ID" -H "$BH")" 404
check "individual reads itself"                "$(code "$DK/customers/$IND_ID"   -H "$IH")" 200
check "individual cannot read an outsider"     "$(code "$DK/customers/$OTHER_ID" -H "$IH")" 404
check "business cannot reach an admin route"   "$(code -X POST "$DK/admin/resync" -H "$BH")" 403
check "individual cannot reach an admin route" "$(code "$DK/admin/treasury" -H "$IH")" 403
check "no token is refused"                    "$(code "$DK/customers")" 401
check "a garbage token is refused"             "$(code "$DK/customers" -H 'authorization: Bearer not.a.token')" 401
curl -sS -X POST "$DK/customers" -H "$BH" -H 'content-type: application/json' \
  -d "{\"name\":\"Forged\",\"customer_type\":\"individual\",\"sub_client_id\":\"$OTHER_ID\"}" \
  | grep -q 'beneath itself' && ok "a forged sub_client_id is refused" || bad "forged sub_client_id"

breathe
section "5. approval gate"
curl -sS -X POST "$DK/accounts" -H "$AH" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$IND_ID\",\"account_type\":\"onramp\",\"destination_asset\":\"USDC\",\"destination_network_id\":\"base-sepolia\",\"source_asset\":\"USD\"}" \
  | grep -q 'not approved to transact' && ok "an unapproved customer cannot open a ramp" || bad "approval gate"

NEW=$(curl -sS -X POST "$DK/admin/sandbox/onboarding" -H "$AH" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$IND_ID\"}" | jq_ "d['new_state']")
check "kyb_approve advances the application" "$NEW" approved

breathe
section "6. ramps"
REC=$(curl -sS -X POST "$DK/customers/$IND_ID/recipients" -H "$AH" -H 'content-type: application/json' \
  -d '{"name":"Smoke recipient"}' | jq_ "d['id']")
DEST=$(curl -sS -X POST "$DK/recipients/$REC/destinations" -H "$AH" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$IND_ID\",\"destination_type\":\"crypto\",\"name\":\"smoke\",\"crypto_address\":\"0xF2e1556b5b41e71244685C6e64e5Dc6C64e1d62B\",\"network_id\":\"base-sepolia\"}" | jq_ "d['id']")

ON=$(curl -sS -X POST "$DK/accounts" -H "$AH" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$IND_ID\",\"account_type\":\"onramp\",\"crypto_destination_id\":\"$DEST\",\"destination_network_id\":\"base-sepolia\",\"source_asset\":\"USD\",\"destination_asset\":\"USDC\"}")
ON_ID=$(echo "$ON" | jq_ "d['id']")
echo "$ON" | jq_ "d['bank_account']['aba_routing_number']" | grep -qE '^[0-9]{9}$' \
  && ok "onramp returns real ACH details" || bad "onramp bank details"

curl -sS -X POST "$DK/accounts" -H "$AH" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$IND_ID\",\"account_type\":\"onramp\",\"destination_asset\":\"DOGE\",\"destination_network_id\":\"base-sepolia\",\"source_asset\":\"USD\"}" \
  | grep -q 'not enabled' && ok "an un-catalogued asset is refused" || bad "catalog allow-list"
curl -sS -X POST "$DK/accounts" -H "$AH" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$IND_ID\",\"account_type\":\"onramp\",\"destination_asset\":\"USDC\",\"destination_network_id\":\"ethereum-mainnet\",\"source_asset\":\"USD\"}" \
  | grep -q 'not permitted' && ok "a mainnet network never leaves the box" || bad "network allow-list"

breathe
section "7. funding and the ledger"
curl -sS -X POST "$DK/admin/sandbox/inbound" -H "$AH" -H 'content-type: application/json' \
  -d "{\"type\":\"ach_inbound\",\"amount\":\"5.00\",\"account_id\":\"$ON_ID\"}" \
  | grep -q 'exceeds the configured cap' && ok "the \$2 sandbox cap is enforced locally" || bad "amount cap"

curl -sS -X POST "$DK/admin/sandbox/inbound" -H "$AH" -H 'content-type: application/json' \
  -d "{\"type\":\"ach_inbound\",\"amount\":\"2.00\",\"account_id\":\"$ON_ID\"}" >/dev/null
ok "deposit simulated"
sleep 6

RS=$(curl -sS -X POST "$DK/admin/resync" -H "$AH")
echo "$RS" | jq_ "d['scanned']" | grep -qE '^[0-9]+$' && ok "resync ran: $(echo "$RS" | jq_ "d")" || bad "resync"

TOT=$(curl -sS "$DK/flows" -H "$AH" | jq_ "sum(t['inbound_minor'] for t in d['totals'])")
[ "${TOT:-0}" -gt 0 ] && ok "inbound value recorded: $TOT minor units" || bad "flows totals are empty"

section "8. webhook authenticity"
check "an unsigned delivery is refused" \
  "$(code -X POST "$DK/webhooks/dakota" -H 'content-type: application/json' -d '{"type":"x"}')" 401
check "a forged signature is refused" \
  "$(code -X POST "$DK/webhooks/dakota" -H 'content-type: application/json' \
      -H 'x-webhook-signature: AAAA' -H "x-webhook-timestamp: $(date +%s)" \
      -H 'x-dakota-event-id: forged' -d '{"amount":"9999"}')" 401
check "a stale timestamp is refused" \
  "$(code -X POST "$DK/webhooks/dakota" -H 'content-type: application/json' \
      -H 'x-webhook-signature: AAAA' -H "x-webhook-timestamp: $(( $(date +%s) - 999 ))" \
      -H 'x-dakota-event-id: stale' -d '{}')" 401

printf '\n\033[1m%d passed, %d failed\033[0m\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
