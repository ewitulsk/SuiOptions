#!/usr/bin/env bash
#
# Fetch per-service JSON secrets from the SOPS/age-encrypted store shipped
# in the deploy bundle (secrets.enc.yaml) and render them into the file
# shapes the binaries expect under /opt/options/<env>/secrets/.
#
# The encrypted store lives in-repo at
# rust-backend/deployment/secrets/<env>.enc.yaml; the deploy bundle drops
# it here as ./secrets.enc.yaml. Decryption needs the age private key at
# /opt/options/age.key (installed once per host, mode 600).
#
# Store layout (one JSON-string value per service per env, keys mirror the
# old AWS Secrets Manager names minus the options/ prefix):
#   options/<env>/indexer    -> {"db_password": "..."}
#   options/<env>/mm-bot     -> {"sui_key": "suiprivkey1...", "quote_key": "..."}
#   options/<env>/scheduler  -> {"sui_key": "suiprivkey1..."}  (deployer key,
#                                holds AdminCap. Named `scheduler` for
#                                historical reasons — the service of that
#                                name is gone; the key itself is still the
#                                canonical publish/admin signer.)
#   options/<env>/cctp-relay -> {"sui_key": "...", "solana_key": "...",
#                                "grpc_url": "..."} (grpc_url is REQUIRED and
#                                must match the relay's configured [sui]
#                                network — see the cctp-relay block below)
#   options/<env>/vault-messenger -> {"sui_key": "...", "evm_key": "...",
#                                "grpc_url": "..."} (grpc_url REQUIRED, same
#                                posture — see the vault-messenger block)
#
# Outputs (consumed by docker-compose):
#   /opt/options/<env>/secrets/mm-bot.toml       (read by mm-bot)
#   /opt/options/<env>/secrets/deployer.toml     (deployer wallet; watched by
#                                                 balance-monitor)
#   /opt/options/<env>/secrets/.db_password      (sourced into .env by deploy.sh)
#
# Idempotent: re-running overwrites the rendered files. Services whose
# secret is absent are silently skipped — that's the supported way
# to opt-out of a service in a given env (e.g. mm-bot in prod).
#
# Requires `jq` and `sops` (installed by deployment/do/host-bootstrap.sh).

set -euo pipefail

ENV="${1:?usage: render-secrets.sh <staging|prod>}"
case "$ENV" in
  staging) NETWORK=testnet ;;
  prod)    NETWORK=testnet ;;
  *) echo "unknown env: $ENV" >&2; exit 1 ;;
esac

DIR="/opt/options/$ENV/secrets"
mkdir -p "$DIR"
chmod 700 "$DIR"

SOPS_FILE="/opt/options/$ENV/secrets.enc.yaml"
export SOPS_AGE_KEY_FILE="${SOPS_AGE_KEY_FILE:-/opt/options/age.key}"
if [ ! -f "$SOPS_FILE" ]; then
  echo "missing $SOPS_FILE (shipped by the deploy bundle)" >&2
  exit 1
fi
if [ ! -f "$SOPS_AGE_KEY_FILE" ]; then
  echo "missing age key at $SOPS_AGE_KEY_FILE" >&2
  exit 1
fi

fetch() {
  # Same contract as the old Secrets Manager fetch: prints the JSON string
  # on stdout, non-zero exit when the key is absent.
  sops decrypt --extract "[\"$ENV/$1\"]" "$SOPS_FILE" 2>/dev/null
}

fetch_global() {
  # Env-independent keys (_master/*, monitoring/*).
  sops decrypt --extract "[\"$1\"]" "$SOPS_FILE" 2>/dev/null
}

# Append a `[pyth]` section with the API key to a rendered secrets TOML, if
# the service's JSON carries a non-empty `pyth_api_key`. Optional by design:
# absent key → anonymous (rate-limited) Pyth tier, no [pyth] section emitted.
#   $1 = service secret JSON   $2 = target .toml path
append_pyth_api_key() {
  local key
  key=$(echo "$1" | jq -r '.pyth_api_key // empty')
  if [ -n "$key" ]; then
    cat >> "$2" <<EOF

[pyth]
api_key = "$key"
EOF
  fi
}

# ---- shared Sui chain endpoints (SO-270, gRPC since SO-336) --------------
# One secret per env: options/<env>/sui-rpc -> {"grpc_url": "...",
# "graphql_url": "..."}. Injected into the [sui] block of the keyed service
# tomls below and rendered as a standalone toml for the keyless services
# (indexer / price-charting / balance-monitor).
#
# JSON-RPC is deactivated on Sui fullnodes, so the legacy `rpc_url` key is
# ignored (the binaries warn if it is still present). Unlike the JSON-RPC
# era, the PUBLIC defaults now work — an absent secret is a normal
# configuration, not a broken one, so this stays a soft fallback.
GRPC_URL=""
GRAPHQL_URL=""
if RPC_JSON=$(fetch sui-rpc 2>/dev/null); then
  GRPC_URL=$(echo "$RPC_JSON" | jq -r '.grpc_url // empty')
  GRAPHQL_URL=$(echo "$RPC_JSON" | jq -r '.graphql_url // empty')
  [ "$GRPC_URL" = "REPLACE_ME" ] && GRPC_URL=""
  [ "$GRAPHQL_URL" = "REPLACE_ME" ] && GRAPHQL_URL=""
fi
# Pre-build the TOML lines so they can be dropped verbatim into the [sui]
# block of each heredoc. Built with escaped quotes here rather than via
# `${VAR:+…}` inline — inside a heredoc that form strips the inner quotes and
# yields invalid TOML. Empty when unset → an inert blank line.
RPC_LINE=""
if [ -n "$GRPC_URL" ]; then
  RPC_LINE="grpc_url = \"$GRPC_URL\""
fi
if [ -n "$GRAPHQL_URL" ]; then
  if [ -n "$RPC_LINE" ]; then
    RPC_LINE="$RPC_LINE
graphql_url = \"$GRAPHQL_URL\""
  else
    RPC_LINE="graphql_url = \"$GRAPHQL_URL\""
  fi
fi
if [ -n "$RPC_LINE" ]; then
  echo "render-secrets: sui-rpc override present"
else
  echo "render-secrets: no sui-rpc override — services use the public endpoints"
fi

# ---- indexer secret -> exported as DB_PASSWORD for compose -----------------
INDEXER_JSON=$(fetch indexer)
DB_PASSWORD=$(echo "$INDEXER_JSON" | jq -r '.db_password')
if [ -z "$DB_PASSWORD" ] || [ "$DB_PASSWORD" = "null" ]; then
  echo "missing db_password in options/$ENV/indexer" >&2
  exit 1
fi
echo "$DB_PASSWORD" > "$DIR/.db_password"
chmod 600 "$DIR/.db_password"

# ---- mm-bot secret -> rendered TOML --------------------------------------
if MM_JSON=$(fetch mm-bot 2>/dev/null); then
  SUI_KEY=$(echo "$MM_JSON" | jq -r '.sui_key')
  QUOTE_KEY=$(echo "$MM_JSON" | jq -r '.quote_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/mm-bot" >&2
    exit 1
  fi
  if [ -z "$QUOTE_KEY" ] || [ "$QUOTE_KEY" = "null" ]; then
    echo "missing quote_key in options/$ENV/mm-bot" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/mm-bot.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE

[mm_bot]
quote_key = "$QUOTE_KEY"
EOF
fi

# ---- staging-mm-bot secret -> rendered TOML ------------------------------
# One key does everything (BalanceManager owner, order signing, gas); no
# quote key. Staging-only: the secret simply doesn't exist in prod, so the
# render is skipped there.
if SMB_JSON=$(fetch staging-mm-bot 2>/dev/null); then
  SUI_KEY=$(echo "$SMB_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/staging-mm-bot" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/staging-mm-bot.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE
EOF
fi

# ---- deployer key -> rendered TOML ---------------------------------------
# The deployer wallet (AdminCap holder) is the canonical publish/admin signer.
# No service reads this file any more — balance-monitor watches it so the
# wallet that funds every redeploy doesn't run dry unnoticed. The AWS secret
# keeps its historical `scheduler` name.
if DEPLOYER_JSON=$(fetch scheduler 2>/dev/null); then
  SUI_KEY=$(echo "$DEPLOYER_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/scheduler" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/deployer.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE
EOF
fi

# ---- cctp-relay secret -> rendered TOML -----------------------------------
# Relayer keys for the CCTP bridge: a Sui key (mints on Sui) and a Solana
# fee-payer keypair (mints on Solana; base58 64-byte or JSON-array format).
# Both must stay funded with gas. Absent secret -> cctp-relay isn't deployed
# in this env.
if CCTP_JSON=$(fetch cctp-relay 2>/dev/null); then
  SUI_KEY=$(echo "$CCTP_JSON" | jq -r '.sui_key')
  SOLANA_KEY=$(echo "$CCTP_JSON" | jq -r '.solana_key')
  # `grpc_url` is the current key; `rpc_url` is accepted as a fallback so an
  # un-migrated secret still renders (its value must now be a gRPC endpoint).
  CCTP_RPC_URL=$(echo "$CCTP_JSON" | jq -r '.grpc_url // .rpc_url // empty')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/cctp-relay" >&2
    exit 1
  fi
  if [ -z "$SOLANA_KEY" ] || [ "$SOLANA_KEY" = "null" ]; then
    echo "missing solana_key in options/$ENV/cctp-relay" >&2
    exit 1
  fi
  # The relay gets its Sui endpoint from its OWN secret, not the shared
  # sui-rpc override: that one is scoped to the env's NETWORK (testnet),
  # while the relay's [sui].network is config-driven and independent of it
  # (mainnet on staging, testnet on prod). Its network is baked into the
  # service config, which is not in the deploy bundle, so this script cannot
  # pick the right shared endpoint — whoever populates the secret can.
  #
  # Required, not optional. Sui deprecated JSON-RPC on public fullnodes on
  # 2026-07-30 and the relay's silent fallback to them took the mainnet USDC
  # bridge down with nothing in the system to say so (SO-320). An absent
  # cctp-relay secret still means "not deployed in this env" and stays quiet;
  # a present secret without an endpoint is a misconfiguration, so fail here
  # rather than at the relay's next restart.
  if [ -z "$CCTP_RPC_URL" ] || [ "$CCTP_RPC_URL" = "REPLACE_ME" ]; then
    echo "missing grpc_url in options/$ENV/cctp-relay — the relay must not fall back to a public Sui fullnode (SO-320)" >&2
    exit 1
  fi
  umask 077
  # Both networks get the key (mirroring [solana] below) — the relay picks
  # the one matching its configured network.
  cat > "$DIR/cctp-relay.toml" <<CCTPEOF
[sui]
testnet = "$SUI_KEY"
mainnet = "$SUI_KEY"
grpc_url = "$CCTP_RPC_URL"

[solana]
devnet = "$SOLANA_KEY"
mainnet = "$SOLANA_KEY"
CCTPEOF
fi

# ---- vault-messenger secret -> rendered TOML ------------------------------
# Relayer keys for the multichain vault messenger: a Sui key (submits the
# hub deliver/handler PTBs — must be endpoint::add_relayer-registered) and
# an EVM key (delivers to the spoke RelayerEndpoint + cranks syncState).
# Both must stay funded with gas. Absent secret -> vault-messenger isn't
# deployed in this env.
if VM_JSON=$(fetch vault-messenger 2>/dev/null); then
  SUI_KEY=$(echo "$VM_JSON" | jq -r '.sui_key')
  EVM_KEY=$(echo "$VM_JSON" | jq -r '.evm_key')
  VM_GRPC_URL=$(echo "$VM_JSON" | jq -r '.grpc_url // empty')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/vault-messenger" >&2
    exit 1
  fi
  if [ -z "$EVM_KEY" ] || [ "$EVM_KEY" = "null" ]; then
    echo "missing evm_key in options/$ENV/vault-messenger" >&2
    exit 1
  fi
  # Same posture as cctp-relay (SO-320): a present secret must carry its
  # own Sui endpoint — no silent fallback to public fullnodes.
  if [ -z "$VM_GRPC_URL" ] || [ "$VM_GRPC_URL" = "REPLACE_ME" ]; then
    echo "missing grpc_url in options/$ENV/vault-messenger — the messenger must not fall back to a public Sui fullnode (SO-320)" >&2
    exit 1
  fi
  umask 077
  # Both networks get the key (mirroring cctp-relay) — the messenger picks
  # the one matching its configured hub network.
  cat > "$DIR/vault-messenger.toml" <<VMEOF
[sui]
testnet = "$SUI_KEY"
mainnet = "$SUI_KEY"
grpc_url = "$VM_GRPC_URL"

[evm]
private_key = "$EVM_KEY"
VMEOF
fi

# ---- auth-service secret -> rendered TOML --------------------------------
# JWT signing secret. auth-service is the sole holder; token-info delegates
# verification to it and never sees this value.
if AUTH_JSON=$(fetch auth-service 2>/dev/null); then
  JWT_SECRET=$(echo "$AUTH_JSON" | jq -r '.jwt_secret')
  if [ -z "$JWT_SECRET" ] || [ "$JWT_SECRET" = "null" ]; then
    echo "missing jwt_secret in options/$ENV/auth-service" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/auth-service.toml" <<EOF
[auth]
jwt_secret = "$JWT_SECRET"
EOF
fi

# ---- gas-station secret -> rendered TOML ---------------------------------
# Sponsor (gas payer) key. One Sui key per env, in the network slot the
# service's config expects (staging/prod → testnet).
if GAS_JSON=$(fetch gas-station 2>/dev/null); then
  SUI_KEY=$(echo "$GAS_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/gas-station" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/gas-station.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE
EOF
fi

# ---- market-sim secret -> rendered TOML ----------------------------------
# The spot-liquidity simulator's dedicated wallet key (SO-302). One Sui key
# per env, in the network slot the service's config expects. Staging-only —
# absent secret -> market-sim isn't deployed in this env, silently skipped.
if MS_JSON=$(fetch market-sim 2>/dev/null); then
  SUI_KEY=$(echo "$MS_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/market-sim" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/market-sim.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE
EOF
fi

# ---- hedge-signer secret -> rendered TOML --------------------------------
# The service's multisig member key (the protocol half of each trading
# vault's 2-of-2 external account). One Sui key per env, in the network
# slot the service's config expects (staging/prod → testnet). Absent
# secret -> hedge-signer isn't provisioned in this env yet; the container
# would crash-loop on the missing file, so create options/<env>/hedge-signer
# with {"sui_key": "suiprivkey1..."} before the first deploy including it.
if HEDGE_JSON=$(fetch hedge-signer 2>/dev/null); then
  SUI_KEY=$(echo "$HEDGE_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/hedge-signer" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/hedge-signer.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE
EOF
fi

# ---- vault-keeper secret -> rendered TOML ---------------------------------
# Plain gas wallet (NOT the deployer key — the keeper holds no capability
# objects; vault.move validates every crank on-chain). One Sui key per
# env, in the network slot the keeper's --network expects (staging/prod
# → testnet; the Dockerfile.keeper APP_ENV mapping mirrors this).
#
# NOTE: unlike the optional services above, the keeper IS declared in
# both envs' compose files — if this secret is absent the container
# crash-loops on a missing /run/secrets/keeper.toml and the deploy's
# health gate rolls the env back. Create `options/<env>/keeper` with
# {"sui_key": "suiprivkey1..."} before the first deploy that includes
# the keeper.
if KEEPER_JSON=$(fetch keeper 2>/dev/null); then
  SUI_KEY=$(echo "$KEEPER_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/keeper" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/keeper.toml" <<EOF
[sui]
$NETWORK = "$SUI_KEY"
$RPC_LINE
EOF
  append_pyth_api_key "$KEEPER_JSON" "$DIR/keeper.toml"
else
  echo "WARNING: options/$ENV/keeper secret not found — keeper will fail its health check if deployed" >&2
fi

# ---- orderbook secret -> rendered TOML -------------------------------------
# Relayer gas wallet for matched-mode settlement (exchange
# settlement::match_orders). Plain wallet, NOT the deployer key and NOT an
# AdminCap holder — the Move package validates every fill against maker
# signatures, so this key can only pay gas and claim crossing prices within
# signed limits. OPTIONAL by design: an absent secret renders no file and
# the service degrades to open-orderbook mode (serves signed orders, no
# matched settlement) rather than crash-looping.
# Layout: options/<env>/orderbook -> {"sui_key": "suiprivkey1..."}
if ORDERBOOK_JSON=$(fetch orderbook 2>/dev/null); then
  OB_SUI_KEY=$(echo "$ORDERBOOK_JSON" | jq -r '.sui_key // empty')
  umask 077
  if [ -n "$OB_SUI_KEY" ] && [ "$OB_SUI_KEY" != "REPLACE_ME" ]; then
    cat > "$DIR/orderbook.toml" <<EOF
[sui]
$NETWORK = "$OB_SUI_KEY"
$RPC_LINE
EOF
  else
    cat > "$DIR/orderbook.toml" <<EOF
[sui]
$RPC_LINE
EOF
  fi
else
  echo "render-secrets: no orderbook secret — service runs open-orderbook mode"
fi

# ---- price-charting secret -> sourced into .env by deploy.sh -------------
# Tiger Data TimescaleDB connection URL. Absent in envs without the
# service — silently skipped like mm-bot.
if CHART_JSON=$(fetch price-charting 2>/dev/null); then
  CHART_DB_URL=$(echo "$CHART_JSON" | jq -r '.database_url')
  if [ -z "$CHART_DB_URL" ] || [ "$CHART_DB_URL" = "null" ] || [ "$CHART_DB_URL" = "REPLACE_ME" ]; then
    echo "missing database_url in options/$ENV/price-charting" >&2
    exit 1
  fi
  umask 077
  echo "$CHART_DB_URL" > "$DIR/.chart_database_url"
  chmod 600 "$DIR/.chart_database_url"
fi

# ---- crossbar secret -> sourced into .env by deploy.sh ---------------------
# RPC endpoints for Switchboard Crossbar: Solana (the devnet queue backs Sui
# testnet feeds; mainnet for when we provision one) and Sui (Queue-object
# reads for its oracle cache, SO-352). Optional — absent secret or absent
# field → compose falls back to the public RPCs.
# Layout: options/<env>/crossbar -> {"solana_devnet_rpc": "...",
#                                    "solana_mainnet_rpc": "...",
#                                    "sui_testnet_rpc": "...",
#                                    "sui_mainnet_rpc": "..."}
if XBAR_JSON=$(fetch crossbar 2>/dev/null); then
  umask 077
  for field in solana_devnet_rpc solana_mainnet_rpc sui_testnet_rpc sui_mainnet_rpc; do
    val=$(echo "$XBAR_JSON" | jq -r --arg f "$field" '.[$f] // empty')
    if [ -n "$val" ] && [ "$val" != "REPLACE_ME" ]; then
      echo "$val" > "$DIR/.$field"
      chmod 600 "$DIR/.$field"
    else
      rm -f "$DIR/.$field"
    fi
  done
fi

# ---- oracle-service secret -> rendered TOML ------------------------------
# The single Pyth gateway (SO-254). Holds the Pyth API key, attached as a
# Bearer header on its one Hermes SSE subscription + Benchmarks requests.
# Absent in envs without the service — silently skipped like mm-bot.
if ORACLE_JSON=$(fetch oracle-service 2>/dev/null); then
  umask 077
  : > "$DIR/oracle-service.toml"
  append_pyth_api_key "$ORACLE_JSON" "$DIR/oracle-service.toml"
fi

# ---- twitter-service secret -> rendered TOML ------------------------------
# Per-account OAuth 1.0a credentials for outgoing tweets (one
# [accounts.<name>] section per account). Staging-only — absent in envs
# without the service, silently skipped like mm-bot.
if TW_JSON=$(fetch twitter-service 2>/dev/null); then
  if ! echo "$TW_JSON" | jq -e '
      (.accounts | type == "object" and length > 0)
      and ([.accounts[] | .api_key, .api_key_secret, .access_token, .access_token_secret]
           | all(type == "string" and length > 0))' >/dev/null; then
    echo "malformed options/$ENV/twitter-service (need accounts.<name>.{api_key,api_key_secret,access_token,access_token_secret})" >&2
    exit 1
  fi
  umask 077
  echo "$TW_JSON" | jq -r '
    .accounts | to_entries[] |
    "[accounts.\(.key)]\n"
    + "api_key             = \"\(.value.api_key)\"\n"
    + "api_key_secret      = \"\(.value.api_key_secret)\"\n"
    + "access_token        = \"\(.value.access_token)\"\n"
    + "access_token_secret = \"\(.value.access_token_secret)\"\n"
  ' > "$DIR/twitter-service.toml"
fi

# ---- social-bot secret -> rendered TOML ------------------------------------
# Slack signing secret + Discord public key (webhook verification).
# Staging-only — absent in envs without the service, silently skipped.
#
# NOTE: social-bot IS health-gated by deploy.sh (via nginx). The service
# refuses to boot on missing/placeholder values, so fill this secret and
# options/<env>/twitter-service with real values before the first deploy
# that includes these services, or the deploy rolls back.
if BOT_JSON=$(fetch social-bot 2>/dev/null); then
  SLACK_SECRET=$(echo "$BOT_JSON" | jq -r '.slack_signing_secret')
  DISCORD_KEY=$(echo "$BOT_JSON" | jq -r '.discord_public_key')
  if [ -z "$SLACK_SECRET" ] || [ "$SLACK_SECRET" = "null" ]; then
    echo "missing slack_signing_secret in options/$ENV/social-bot" >&2
    exit 1
  fi
  if [ -z "$DISCORD_KEY" ] || [ "$DISCORD_KEY" = "null" ]; then
    echo "missing discord_public_key in options/$ENV/social-bot" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/social-bot.toml" <<EOF
slack_signing_secret = "$SLACK_SECRET"
discord_public_key   = "$DISCORD_KEY"
EOF
fi

# ---- Spaces key -> sourced into .env by deploy.sh --------------------------
# S3-compatible object-store credentials (DigitalOcean Spaces) for
# api-service's /analytics lake reads. object_store's AmazonS3Builder
# reads them from AWS_* env vars, which compose maps from SPACES_*.
if SPACES_JSON=$(fetch_global "_master/spaces"); then
  umask 077
  for field in access_key secret_key endpoint region; do
    val=$(echo "$SPACES_JSON" | jq -r --arg f "$field" '.[$f] // empty')
    if [ -n "$val" ]; then
      echo "$val" > "$DIR/.spaces_$field"
      chmod 600 "$DIR/.spaces_$field"
    fi
  done
fi

# ---- keyless services -> standalone [sui] endpoint toml -------------------
# indexer / price-charting / balance-monitor hold no signing key but still
# build a chain client. They read only the `[sui]` endpoint keys from these
# files (mounted at /run/secrets/<svc>.toml). Rendered only when an override
# is present — absent file → those services fall back to their config / the
# public endpoints.
if [ -n "$RPC_LINE" ]; then
  umask 077
  for svc in indexer price-charting balance-monitor; do
    cat > "$DIR/$svc.toml" <<EOF
[sui]
$RPC_LINE
EOF
  done
fi

echo "render-secrets: ok ($ENV)"
