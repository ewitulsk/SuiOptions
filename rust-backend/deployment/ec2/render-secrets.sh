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

# ---- oracle-service secret -> rendered TOML ------------------------------
# The single Pyth gateway (SO-254). Holds the Pyth API key, attached as a
# Bearer header on its one Hermes SSE subscription + Benchmarks requests.
# Absent in envs without the service — silently skipped like mm-bot.
if ORACLE_JSON=$(fetch oracle-service 2>/dev/null); then
  umask 077
  : > "$DIR/oracle-service.toml"
  append_pyth_api_key "$ORACLE_JSON" "$DIR/oracle-service.toml"
fi

# ---- solana-indexer secret -> rendered TOML --------------------------------
# Helius API key for the LaserStream subscription. Required by the service
# (no public fallback); an absent or unfilled secret is skipped so it never
# blocks other services' deploys — solana-indexer itself then crash-loops
# until options/$ENV/solana-indexer (key helius_api_key) is filled.
if SOLANA_INDEXER_JSON=$(fetch solana-indexer 2>/dev/null); then
  HELIUS_API_KEY=$(echo "$SOLANA_INDEXER_JSON" | jq -r '.helius_api_key')
  if [ -n "$HELIUS_API_KEY" ] && [ "$HELIUS_API_KEY" != "null" ] && [ "$HELIUS_API_KEY" != "REPLACE_ME" ]; then
    umask 077
    cat > "$DIR/solana-indexer.toml" <<EOF
[helius]
api_key = "$HELIUS_API_KEY"
EOF
  else
    echo "render-secrets: options/$ENV/solana-indexer unfilled (helius_api_key) — skipped" >&2
  fi
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

# ═══ Solana stack (docs/solana/backend/13-infra-and-deployment.md §7) ═══════

# ---- shared Solana JSON-RPC endpoint ---------------------------------------
# Mirrors the sui-rpc pattern above: one secret per env
# (options/<env>/solana-rpc -> {"rpc_url": "..."}), injected into the
# [solana] block of the keyed Solana service tomls below and rendered as a
# standalone toml for solana-balance-monitor. Absent or REPLACE_ME →
# SOLANA_RPC_URL stays empty and the services fall back to the public
# cluster endpoint.
SOLANA_RPC_URL=""
if SOLANA_RPC_JSON=$(fetch solana-rpc 2>/dev/null); then
  SOLANA_RPC_URL=$(echo "$SOLANA_RPC_JSON" | jq -r '.rpc_url // empty')
  if [ "$SOLANA_RPC_URL" = "REPLACE_ME" ]; then
    SOLANA_RPC_URL=""
  fi
fi
# Pre-built TOML line, same reason as RPC_LINE above (heredoc quoting).
SOLANA_RPC_LINE=""
if [ -n "$SOLANA_RPC_URL" ]; then
  SOLANA_RPC_LINE="rpc_url = \"$SOLANA_RPC_URL\""
  echo "render-secrets: solana-rpc override present"
else
  echo "render-secrets: no solana-rpc override — Solana services use public RPC"
fi

# Network slot the keyed Solana service tomls use. BOTH envs run devnet
# today (prod is a distinct devnet deployment); flip to mainnet-beta
# together with the service configs + Dockerfile --network mappings.
SOLANA_NETWORK=devnet

# ---- solana-auth-service secret -> rendered TOML ---------------------------
# JWT signing secret (terraform auto-generated, never REPLACE_ME).
# solana-auth-service is the sole holder — deliberately distinct from the
# Sui auth-service secret so tokens aren't cross-valid between domains.
if SOLANA_AUTH_JSON=$(fetch solana-auth-service 2>/dev/null); then
  JWT_SECRET=$(echo "$SOLANA_AUTH_JSON" | jq -r '.jwt_secret')
  if [ -z "$JWT_SECRET" ] || [ "$JWT_SECRET" = "null" ]; then
    echo "missing jwt_secret in options/$ENV/solana-auth-service" >&2
    exit 1
  fi
  umask 077
  cat > "$DIR/solana-auth-service.toml" <<EOF
[auth]
jwt_secret = "$JWT_SECRET"
EOF
fi

# ---- solana-gas-station secret -> rendered TOML ----------------------------
# Station (fee payer + faucet mint authority) keypair, in the network slot
# the service's config expects. Unfilled (REPLACE_ME) secrets are skipped
# like solana-indexer's so they never block other services' deploys — the
# service then crash-loops until options/$ENV/solana-gas-station (key:
# keypair) is filled.
if SOLANA_GAS_JSON=$(fetch solana-gas-station 2>/dev/null); then
  KEYPAIR=$(echo "$SOLANA_GAS_JSON" | jq -r '.keypair // empty')
  if [ -n "$KEYPAIR" ] && [ "$KEYPAIR" != "REPLACE_ME" ]; then
    umask 077
    cat > "$DIR/solana-gas-station.toml" <<EOF
[solana]
$SOLANA_NETWORK = "$KEYPAIR"
$SOLANA_RPC_LINE
EOF
  else
    echo "render-secrets: options/$ENV/solana-gas-station unfilled (keypair) — skipped" >&2
  fi
fi

# ---- solana-scheduler secret -> rendered TOML ------------------------------
# The scheduler signs with the admin keypair (config.admin — the parallel
# of the Sui deployer key). Same skip-if-unfilled posture as above.
if SOLANA_SCH_JSON=$(fetch solana-scheduler 2>/dev/null); then
  KEYPAIR=$(echo "$SOLANA_SCH_JSON" | jq -r '.keypair // empty')
  if [ -n "$KEYPAIR" ] && [ "$KEYPAIR" != "REPLACE_ME" ]; then
    umask 077
    cat > "$DIR/solana-scheduler.toml" <<EOF
[solana]
$SOLANA_NETWORK = "$KEYPAIR"
$SOLANA_RPC_LINE
EOF
  else
    echo "render-secrets: options/$ENV/solana-scheduler unfilled (keypair) — skipped" >&2
  fi
fi

# ---- solana-keeper secret -> rendered TOML ---------------------------------
# Plain gas wallet (no privileged accounts — every crank is validated
# on-chain) + an optional Pyth API key for the keeper's direct Hermes path.
if SOLANA_KEEPER_JSON=$(fetch solana-keeper 2>/dev/null); then
  KEYPAIR=$(echo "$SOLANA_KEEPER_JSON" | jq -r '.keypair // empty')
  if [ -n "$KEYPAIR" ] && [ "$KEYPAIR" != "REPLACE_ME" ]; then
    umask 077
    cat > "$DIR/solana-keeper.toml" <<EOF
[solana]
$SOLANA_NETWORK = "$KEYPAIR"
$SOLANA_RPC_LINE
EOF
    # Optional [pyth] api_key — the placeholder REPLACE_ME counts as absent
    # (absent → anonymous rate-limited Hermes tier).
    SOLANA_KEEPER_PYTH=$(echo "$SOLANA_KEEPER_JSON" | jq -r '.pyth_api_key // empty')
    if [ -n "$SOLANA_KEEPER_PYTH" ] && [ "$SOLANA_KEEPER_PYTH" != "REPLACE_ME" ]; then
      cat >> "$DIR/solana-keeper.toml" <<EOF

[pyth]
api_key = "$SOLANA_KEEPER_PYTH"
EOF
    fi
  else
    echo "render-secrets: options/$ENV/solana-keeper unfilled (keypair) — skipped" >&2
  fi
fi

# ---- solana-mm-bot secret -> rendered TOML ---------------------------------
# Wallet keypair + the ed25519 quote signing seed registered on the
# MmAccount. Same skip-if-unfilled posture as above.
if SOLANA_MM_JSON=$(fetch solana-mm-bot 2>/dev/null); then
  KEYPAIR=$(echo "$SOLANA_MM_JSON" | jq -r '.keypair // empty')
  QUOTE_KEY=$(echo "$SOLANA_MM_JSON" | jq -r '.quote_key // empty')
  if [ -n "$KEYPAIR" ] && [ "$KEYPAIR" != "REPLACE_ME" ] \
    && [ -n "$QUOTE_KEY" ] && [ "$QUOTE_KEY" != "REPLACE_ME" ]; then
    umask 077
    cat > "$DIR/solana-mm-bot.toml" <<EOF
[solana]
$SOLANA_NETWORK = "$KEYPAIR"
$SOLANA_RPC_LINE

[mm_bot]
quote_key = "$QUOTE_KEY"
EOF
  else
    echo "render-secrets: options/$ENV/solana-mm-bot unfilled (keypair/quote_key) — skipped" >&2
  fi
fi

# ---- solana-oracle-service secret -> rendered TOML -------------------------
# The Solana stack's single Pyth gateway (mirror of oracle-service above).
# The API key is optional: an unfilled (REPLACE_ME) key renders an empty
# toml → anonymous rate-limited tier, no [pyth] section.
if SOLANA_ORACLE_JSON=$(fetch solana-oracle-service 2>/dev/null); then
  umask 077
  : > "$DIR/solana-oracle-service.toml"
  SOLANA_ORACLE_PYTH=$(echo "$SOLANA_ORACLE_JSON" | jq -r '.pyth_api_key // empty')
  if [ -n "$SOLANA_ORACLE_PYTH" ] && [ "$SOLANA_ORACLE_PYTH" != "REPLACE_ME" ]; then
    cat >> "$DIR/solana-oracle-service.toml" <<EOF
[pyth]
api_key = "$SOLANA_ORACLE_PYTH"
EOF
  fi
fi

# ---- solana-price-charting secret -> sourced into .env by deploy.sh --------
# Tiger Data TimescaleDB connection URL for the Solana OHLC/APY store
# (compose injects it as SOLANA_CHART_DATABASE_URL). Mirrors the Sui
# price-charting flow above, including the fail-noisy posture on an
# unfilled secret.
if SOLANA_CHART_JSON=$(fetch solana-price-charting 2>/dev/null); then
  SOLANA_CHART_DB_URL=$(echo "$SOLANA_CHART_JSON" | jq -r '.database_url')
  if [ -z "$SOLANA_CHART_DB_URL" ] || [ "$SOLANA_CHART_DB_URL" = "null" ] || [ "$SOLANA_CHART_DB_URL" = "REPLACE_ME" ]; then
    echo "missing database_url in options/$ENV/solana-price-charting" >&2
    exit 1
  fi
  umask 077
  echo "$SOLANA_CHART_DB_URL" > "$DIR/.solana_chart_database_url"
  chmod 600 "$DIR/.solana_chart_database_url"
fi

# ---- keyless Solana services -> standalone [solana] rpc_url toml -----------
# solana-balance-monitor holds no signing key of its own (it reads the
# sibling services' rendered files for watch addresses) but still builds an
# RPC client; its --secrets file carries only the shared override. Rendered
# only when the override is present — absent file → public cluster RPC.
if [ -n "$SOLANA_RPC_URL" ]; then
  umask 077
  cat > "$DIR/solana-balance-monitor.toml" <<EOF
[solana]
rpc_url = "$SOLANA_RPC_URL"
EOF
fi

# NOTE: solana-token-info renders nothing here — like the Sui token-info it
# authenticates to Postgres with the shared per-env role password
# (.db_password above), injected by deploy.sh as DB_PASSWORD.

echo "render-secrets: ok ($ENV)"
