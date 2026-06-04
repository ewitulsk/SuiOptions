#!/usr/bin/env sh
#
# Wipe + (idempotently) provision a service's Postgres database for an env.
#
# Runs ON the EC2 host (invoked over SSM by the wipe-provision-db and
# redeploy-contract workflows). Pure DB work — it does NOT stop or start
# the service container; the caller owns service lifecycle.
#
# What it does, in order:
#   1. Create the per-env role `<service>_<env>` if it doesn't exist.
#   2. Create the database `<service>_<env>` (owned by that role) if it
#      doesn't exist.
#   3. Reset the `public` schema (DROP CASCADE + CREATE) and re-grant it
#      to the role.
# Steps 1–2 make first-time provisioning idempotent; step 3 is the wipe.
# Service binaries re-run their embedded migrations on next start.
#
# Usage:
#   DB_HOST=<rds-host> MASTER_USER=<u> MASTER_PASS=<p> \
#     sh wipe-provision-db.sh <env> <service>
#
#   <env>     dev | staging        (prod is BLOCKED)
#   <service> indexer | scheduler | token-info
#
# Master creds come from the caller (the deploy role reads
# options/_master/db) — the per-env role can't CREATE on the cluster. The
# per-env role password is read from the box at
# /opt/options/<env>/secrets/.db_password (written by render-secrets.sh;
# shared by every service in the env).
#
# SSM runs this under /bin/sh (dash). Keep it POSIX — no bashisms.

set -eu

ENV="${1:?usage: wipe-provision-db.sh <env> <service>}"
SERVICE="${2:?usage: wipe-provision-db.sh <env> <service>}"

# ── PRODUCTION GUARD ──────────────────────────────────────────────────
case "$ENV" in
  prod|production|mainnet)
    echo "FATAL: refusing to touch production"
    exit 1
    ;;
  dev|staging) : ;;
  *)
    echo "FATAL: unknown env '$ENV' (expected dev|staging)"
    exit 1
    ;;
esac

# DB name and owning role both follow the `<service>_<env>` convention.
case "$SERVICE" in
  indexer)    DB_PREFIX=indexer ;;
  scheduler)  DB_PREFIX=scheduler ;;
  token-info) DB_PREFIX=token_info ;;
  *)
    echo "FATAL: unknown service '$SERVICE' (expected indexer|scheduler|token-info)"
    exit 1
    ;;
esac

DB_NAME="${DB_PREFIX}_${ENV}"
DB_ROLE="${DB_PREFIX}_${ENV}"

: "${DB_HOST:?DB_HOST must be set}"
: "${MASTER_USER:?MASTER_USER must be set}"
: "${MASTER_PASS:?MASTER_PASS must be set}"

# Per-env role password (shared across services in the env).
ROLE_PASS_FILE="/opt/options/$ENV/secrets/.db_password"
if [ ! -r "$ROLE_PASS_FILE" ]; then
  echo "FATAL: $ROLE_PASS_FILE not found — run render-secrets.sh first"
  exit 1
fi
ROLE_PASS=$(cat "$ROLE_PASS_FILE")

# psql as the RDS master against a target db. SQL arrives on stdin.
# `-i` keeps stdin open for the heredoc; ON_ERROR_STOP aborts on the
# first SQL error so a half-applied reset can't pass silently.
psql_master() {
  _db="$1"
  shift
  docker run --rm -i --network host -e PGPASSWORD="$MASTER_PASS" postgres:16 \
    psql -v ON_ERROR_STOP=1 -h "$DB_HOST" -U "$MASTER_USER" -d "$_db" "$@"
}

echo "── ensuring role $DB_ROLE ──"
# format()+\gexec so the CREATE only runs when absent; %I/%L quote the
# identifier and the password literal safely.
psql_master postgres -v role="$DB_ROLE" -v rolepass="$ROLE_PASS" <<'SQL'
SELECT format('CREATE ROLE %I LOGIN PASSWORD %L', :'role', :'rolepass')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = :'role')\gexec
SQL

echo "── ensuring database $DB_NAME (owner $DB_ROLE) ──"
# CREATE DATABASE can't run inside a txn block; \gexec runs it standalone.
psql_master postgres -v db="$DB_NAME" -v role="$DB_ROLE" <<'SQL'
SELECT format('CREATE DATABASE %I OWNER %I', :'db', :'role')
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = :'db')\gexec
SQL

echo "── wiping public schema in $DB_NAME ──"
psql_master "$DB_NAME" -v role="$DB_ROLE" <<'SQL'
DROP SCHEMA IF EXISTS public CASCADE;
CREATE SCHEMA public;
GRANT ALL ON SCHEMA public TO :"role";
SQL

echo "wipe-provision-db: ok ($SERVICE / $ENV → $DB_NAME)"
