#!/usr/bin/env bash
#
# Fetch per-service JSON secrets from AWS Secrets Manager and render them
# into the file shapes the binaries expect under
# /opt/options/<env>/secrets/.
#
# Secrets Manager layout (one JSON entry per service per env):
#   options/<env>/indexer  -> {"db_password": "..."}
#   options/<env>/mm-bot   -> {"sui_key": "suiprivkey1...", "quote_key": "..."}
#
# Outputs (consumed by docker-compose):
#   /opt/options/<env>/secrets/mm-bot.toml   (read by mm-bot via /run/secrets)
#   /opt/options/<env>/.env line: DB_PASSWORD=<indexer.db_password>
#
# Requires `jq` and `aws` (installed by ec2-bootstrap.sh).

set -euo pipefail

ENV="${1:?usage: render-secrets.sh <dev|staging|prod>}"
case "$ENV" in
  dev)     NETWORK=testnet ;;
  staging) NETWORK=devnet  ;;
  prod)    NETWORK=mainnet ;;
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
# Skip if the secret doesn't exist (prod intentionally has no mm-bot for now).
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

echo "render-secrets: ok ($ENV)"
