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

ENV="${1:?usage: render-secrets.sh <dev|staging|prod>}"
case "$ENV" in
  dev)     NETWORK=devnet  ;;
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
# service's config expects (dev → devnet, staging/prod → testnet).
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
EOF
fi

echo "render-secrets: ok ($ENV)"
