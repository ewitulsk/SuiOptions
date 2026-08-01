#!/usr/bin/env bash
#
# Fetch per-service JSON secrets from AWS Secrets Manager and render them
# into the file shapes the binaries expect under
# /opt/options/<env>/secrets/.
#
# Secrets Manager layout (one JSON entry per service per env):
#   options/<env>/indexer    -> {"db_password": "..."}
#   options/<env>/mm-bot     -> {"sui_key": "suiprivkey1...", "quote_key": "..."}
#   options/<env>/scheduler  -> {"sui_key": "suiprivkey1..."}  (deployer key,
#                                holds AdminCap; absent if scheduler isn't
#                                deployed in this env)
#   options/<env>/cctp-relay -> {"sui_key": "...", "solana_key": "...",
#                                "rpc_url": "..."}  (rpc_url is REQUIRED and
#                                must match the relay's configured [sui]
#                                network — see the cctp-relay block below)
#
# Outputs (consumed by docker-compose):
#   /opt/options/<env>/secrets/mm-bot.toml       (read by mm-bot)
#   /opt/options/<env>/secrets/scheduler.toml    (read by option-scheduler)
#   /opt/options/<env>/secrets/.db_password      (sourced into .env by deploy.sh)
#
# Idempotent: re-running overwrites the rendered files. Services whose
# AWS secret is absent are silently skipped — that's the supported way
# to opt-out of a service in a given env (e.g. mm-bot in prod).
#
# Requires `jq` and `aws` (installed by ec2-bootstrap.sh).

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

fetch() {
  aws secretsmanager get-secret-value \
    --secret-id "options/$ENV/$1" \
    --query SecretString --output text
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

# ---- shared Sui JSON-RPC endpoint (SO-270) -------------------------------
# One secret per env (options/<env>/sui-rpc -> {"rpc_url": "..."}). Injected
# into the [sui] block of the keyed service tomls below and rendered as a
# standalone toml for the keyless services (indexer / price-charting /
# balance-monitor). Absent or REPLACE_ME → RPC_URL stays empty and every
# service falls back to the public Sui endpoint (resolve_rpc_url degrades
# gracefully — never a hard fail).
RPC_URL=""
if RPC_JSON=$(fetch sui-rpc 2>/dev/null); then
  RPC_URL=$(echo "$RPC_JSON" | jq -r '.rpc_url // empty')
  if [ "$RPC_URL" = "REPLACE_ME" ]; then
    RPC_URL=""
  fi
fi
# Pre-build the TOML line so it can be dropped verbatim into the [sui] block of
# each heredoc. Built with escaped quotes here rather than via `${RPC_URL:+…}`
# inline — inside a heredoc that form strips the inner quotes and yields
# invalid TOML. Empty when unset → an inert blank line in the rendered file.
RPC_LINE=""
if [ -n "$RPC_URL" ]; then
  RPC_LINE="rpc_url = \"$RPC_URL\""
  echo "render-secrets: sui-rpc override present"
else
  echo "render-secrets: no sui-rpc override — services use public RPC"
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

# ---- option-scheduler secret -> rendered TOML ----------------------------
# The scheduler signs with the deployer key (AdminCap holder). One Sui
# key per env; no quote key (scheduler doesn't sign quotes).
if SCH_JSON=$(fetch scheduler 2>/dev/null); then
  SUI_KEY=$(echo "$SCH_JSON" | jq -r '.sui_key')
  if [ -z "$SUI_KEY" ] || [ "$SUI_KEY" = "null" ]; then
    echo "missing sui_key in options/$ENV/scheduler" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/scheduler.toml" <<EOF
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
  CCTP_RPC_URL=$(echo "$CCTP_JSON" | jq -r '.rpc_url // empty')
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
    echo "missing rpc_url in options/$ENV/cctp-relay — the relay must not fall back to a public Sui fullnode (SO-320)" >&2
    exit 1
  fi
  umask 077
  # Both networks get the key (mirroring [solana] below) — the relay picks
  # the one matching its configured network.
  cat > "$DIR/cctp-relay.toml" <<CCTPEOF
[sui]
testnet = "$SUI_KEY"
mainnet = "$SUI_KEY"
rpc_url = "$CCTP_RPC_URL"

[solana]
devnet = "$SOLANA_KEY"
mainnet = "$SOLANA_KEY"
CCTPEOF
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
# Solana RPC endpoints for Switchboard Crossbar (the devnet queue backs Sui
# testnet feeds; mainnet for when we provision one). Optional — absent
# secret or absent field → compose falls back to the public RPCs.
# Layout: options/<env>/crossbar -> {"solana_devnet_rpc": "...",
#                                    "solana_mainnet_rpc": "..."}
if XBAR_JSON=$(fetch crossbar 2>/dev/null); then
  umask 077
  for field in solana_devnet_rpc solana_mainnet_rpc; do
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

# ---- keyless services -> standalone [sui] rpc_url toml --------------------
# indexer / price-charting / balance-monitor hold no signing key but still
# build a SuiClient. They read only `[sui] rpc_url` from these files (mounted
# at /run/secrets/<svc>.toml). Rendered only when the override is present —
# absent file → those services fall back to their config / public RPC.
if [ -n "$RPC_URL" ]; then
  umask 077
  for svc in indexer price-charting balance-monitor; do
    cat > "$DIR/$svc.toml" <<EOF
[sui]
rpc_url = "$RPC_URL"
EOF
  done
fi

echo "render-secrets: ok ($ENV)"
