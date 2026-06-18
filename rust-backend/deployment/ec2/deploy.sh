#!/usr/bin/env bash
#
# Selective deploy entry point. Pulls images for and rolls only the
# services named in the SERVICES_JSON argument, leaving the rest of the
# env's compose stack untouched.
#
# Invoked by GitHub Actions via `aws ssm send-command`:
#
#   deploy.sh <staging|prod> '["indexer","mm-bot"]'
#
# - IMAGE_TAG is the tag the planned services move to (one tag per
#   deploy; per-service rollback uses the workflow's image_tag dispatch
#   input on a force_all run).
# - Services already declared in this env's compose file but NOT in the
#   request keep their currently-deployed tag — .env carries one
#   <SERVICE>_TAG line per declared service across deploys.
# - Services in the request but NOT declared in this env's compose file
#   are logged + skipped (e.g. asking for mm-bot in prod today).
# - Health-check + rollback runs for each planned service that exposes
#   /health (see health_path_for() for the current set). Services without
#   a /health endpoint are skipped — extend health_path_for() when adding
#   one.

set -euo pipefail

ENV="${1:?usage: deploy.sh <staging|prod> <services-json>}"
SERVICES_JSON="${2:?services-json argument required, e.g. '[\"indexer\"]'}"

case "$ENV" in
  staging|prod) ;;
  *) echo "unknown env: $ENV" >&2; exit 1 ;;
esac

cd "/opt/options/$ENV"

# One-off cleanup: pre-SO-54 bundles dropped nginx.conf flat in the env
# dir. After SO-54 the file lives under nginx/nginx.conf and the old
# top-level file is never touched again. Idempotent — `rm -f` on a
# missing path is a no-op — so this is safe to leave in across all
# future deploys.
rm -f "./nginx.conf"

: "${IMAGE_TAG:?IMAGE_TAG must be set}"
: "${ECR:?ECR must be set}"
: "${DB_HOST:?DB_HOST must be set}"
: "${AWS_REGION:?AWS_REGION must be set}"

COMPOSE_FILE="docker-compose.${ENV}.yml"

# Canonical service set + their .env tag-variable names + the compose
# service name (mostly identical to the cargo crate name, except
# quoting-service is referenced as `quoting` in compose).
ALL_SERVICES=(indexer quoting-service mm-bot option-scheduler api-service token-info auth-service gas-station price-charting balance-monitor keeper)

tag_var_for() {
  case "$1" in
    indexer)          echo INDEXER_TAG ;;
    quoting-service)  echo QUOTING_TAG ;;
    mm-bot)           echo MM_BOT_TAG ;;
    option-scheduler) echo SCHEDULER_TAG ;;
    api-service)      echo API_SERVICE_TAG ;;
    token-info)       echo TOKEN_INFO_TAG ;;
    auth-service)     echo AUTH_SERVICE_TAG ;;
    gas-station)      echo GAS_STATION_TAG ;;
    price-charting)   echo PRICE_CHARTING_TAG ;;
    balance-monitor)  echo BALANCE_MONITOR_TAG ;;
    keeper)           echo KEEPER_TAG ;;
    *) return 1 ;;
  esac
}
compose_name_for() {
  case "$1" in
    indexer)          echo indexer ;;
    quoting-service)  echo quoting ;;
    mm-bot)           echo mm-bot ;;
    option-scheduler) echo option-scheduler ;;
    api-service)      echo api-service ;;
    token-info)       echo token-info ;;
    auth-service)     echo auth-service ;;
    gas-station)      echo gas-station ;;
    price-charting)   echo price-charting ;;
    balance-monitor)  echo balance-monitor ;;
    keeper)           echo keeper ;;
    *) return 1 ;;
  esac
}

read_env_var() {
  if [ -f .env ]; then
    awk -F= -v k="$1" '$1 == k { v=$0; sub(/^[^=]*=/, "", v); print v }' .env | tail -1
  fi
}

# Parse requested services.
mapfile -t REQUESTED < <(echo "$SERVICES_JSON" | jq -r '.[]')
if [ "${#REQUESTED[@]}" -eq 0 ]; then
  echo "deploy.sh: no services requested; exiting"
  exit 0
fi

# Filter to services declared in this env's compose file.
DECLARED=$(docker compose -f "$COMPOSE_FILE" config --services 2>/dev/null || true)
PLANNED=()
SKIPPED=()
for svc in "${REQUESTED[@]}"; do
  if ! cname=$(compose_name_for "$svc" 2>/dev/null); then
    SKIPPED+=("$svc(unknown)")
    continue
  fi
  if printf '%s\n' "$DECLARED" | grep -qx "$cname"; then
    PLANNED+=("$svc")
  else
    SKIPPED+=("$svc(absent-in-$ENV)")
  fi
done
if [ "${#SKIPPED[@]}" -gt 0 ]; then
  echo "skipping: ${SKIPPED[*]}"
fi
if [ "${#PLANNED[@]}" -eq 0 ]; then
  echo "deploy.sh: nothing to roll in $ENV"
  exit 0
fi

echo "rolling in $ENV @ $IMAGE_TAG: ${PLANNED[*]}"

# Render secrets (idempotent; renders only services with an AWS secret).
./render-secrets.sh "$ENV"
DB_PASSWORD=$(cat "secrets/.db_password")
# Tiger Data TimescaleDB URL for price-charting; absent in envs where the
# service isn't provisioned (the compose entry is absent there too).
CHART_DATABASE_URL=""
if [ -f "secrets/.chart_database_url" ]; then
  CHART_DATABASE_URL=$(cat "secrets/.chart_database_url")
fi

# OTLP trace endpoint (SO-180). On the shared host the services reach the
# co-located Tempo by docker DNS; the dedicated prod host gets the central
# Tempo's VPC address dropped in `otel-endpoint` by cloud-init.
OTEL_ENDPOINT="http://tempo:4318"
if [ -f "otel-endpoint" ]; then
  OTEL_ENDPOINT=$(cat "otel-endpoint")
fi

# Snapshot prior tags for rollback.
declare -A PREV_TAG
for svc in "${PLANNED[@]}"; do
  PREV_TAG[$svc]=$(read_env_var "$(tag_var_for "$svc")")
done

# Build new .env: carry prior tags for un-rolled services, overlay
# IMAGE_TAG on the planned ones.
declare -A NEW_TAG
for svc in "${ALL_SERVICES[@]}"; do
  v=$(tag_var_for "$svc")
  NEW_TAG[$v]=$(read_env_var "$v")
done
for svc in "${PLANNED[@]}"; do
  NEW_TAG[$(tag_var_for "$svc")]="$IMAGE_TAG"
done

NEW_ENV=$(mktemp ./.env.new.XXXXXX)
trap 'rm -f "$NEW_ENV"' EXIT
{
  echo "ECR=$ECR"
  echo "DB_PASSWORD=$DB_PASSWORD"
  echo "DB_HOST=$DB_HOST"
  if [ -n "$CHART_DATABASE_URL" ]; then
    echo "CHART_DATABASE_URL=$CHART_DATABASE_URL"
  fi
  echo "OTEL_ENDPOINT=$OTEL_ENDPOINT"
  for svc in "${ALL_SERVICES[@]}"; do
    v=$(tag_var_for "$svc")
    val="${NEW_TAG[$v]:-}"
    if [ -n "$val" ]; then
      echo "$v=$val"
    fi
  done
} > "$NEW_ENV"
mv "$NEW_ENV" .env
trap - EXIT

# Validate that every declared service in compose has a tag in .env.
# A fresh box (or a service newly added to compose) needs at least one
# force_all deploy to seed all the tags before partial deploys work.
for svc in "${ALL_SERVICES[@]}"; do
  cname=$(compose_name_for "$svc")
  if printf '%s\n' "$DECLARED" | grep -qx "$cname"; then
    v=$(tag_var_for "$svc")
    if [ -z "$(read_env_var "$v")" ]; then
      echo "ERROR: $svc is declared in $COMPOSE_FILE but no $v in .env" >&2
      echo "       run a force_all deploy first to seed every service's tag" >&2
      exit 2
    fi
  fi
done

# ECR auth + pull + up only the planned services.
aws ecr get-login-password --region "$AWS_REGION" \
  | docker login --username AWS --password-stdin "$ECR"

PLANNED_CNAMES=()
for svc in "${PLANNED[@]}"; do
  PLANNED_CNAMES+=("$(compose_name_for "$svc")")
done

docker compose -f "$COMPOSE_FILE" pull "${PLANNED_CNAMES[@]}"
docker compose -f "$COMPOSE_FILE" up -d "${PLANNED_CNAMES[@]}"

# nginx is the single public entrypoint per env and is NOT in
# ALL_SERVICES — its image is a pinned public tag and it's not part of
# the rollable set. Ensure it's running every deploy (idempotent: compose
# no-ops if the container is already up with the same spec). The reload
# below picks up any bind-mount nginx.conf changes the bundle dropped on
# disk, since `up -d` alone won't.
docker compose -f "$COMPOSE_FILE" up -d nginx

# Health-check every planned service that has a /health endpoint. Probes
# through nginx — no service publishes a host port anymore, so the only
# path from the host is via the env's nginx sidecar. Verifies both that
# nginx is up AND that the upstream is reachable + healthy.
#
# Returns the public nginx path for $1, or non-zero if that service has no
# /health endpoint and should be skipped.
health_path_for() {
  case "$1" in
    quoting-service)  echo "/$ENV/quoting/health" ;;
    api-service)      echo "/$ENV/api/health" ;;
    indexer)          echo "/$ENV/indexer/health" ;;
    option-scheduler) echo "/$ENV/scheduler/health" ;;
    mm-bot)           echo "/$ENV/mm-bot/health" ;;
    token-info)       echo "/$ENV/token-info/health" ;;
    auth-service)     echo "/$ENV/auth/health" ;;
    price-charting)   echo "/$ENV/charts/health" ;;
    keeper)           echo "/$ENV/keeper/health" ;;
    *) return 1 ;;
  esac
}

# Per-service health-probe budget, in attempts (each attempt is followed by a
# 2s sleep). Most services answer /health within seconds, so 30 (~60s) is
# plenty. option-scheduler is the exception on a contract redeploy: its DB is
# wiped, so on first boot it creates DeepBook pools + vaults + rolls on-chain
# before it settles, which can take a few minutes. Give it a wider window so a
# redeploy doesn't roll the whole stack back on a near-miss timeout.
health_attempts_for() {
  case "$1" in
    option-scheduler) echo 150 ;;  # ~5 min
    *)                echo 30 ;;   # ~60s
  esac
}

rollback() {
  echo "rolling planned services back to prior tags" >&2
  for svc in "${PLANNED[@]}"; do
    v=$(tag_var_for "$svc")
    prev="${PREV_TAG[$svc]:-}"
    if [ -n "$prev" ]; then
      sed -i "s|^$v=.*|$v=$prev|" .env
    fi
  done
  docker compose -f "$COMPOSE_FILE" up -d "${PLANNED_CNAMES[@]}" || true
}

# Validate the new nginx.conf and signal a graceful reload before
# probing health, otherwise new locations added in this deploy would
# 404 against the still-in-memory previous config. The config lives at
# the -c path the nginx master was started with (see compose), not
# /etc/nginx/nginx.conf — point `nginx -t` there explicitly.
if ! docker compose -f "$COMPOSE_FILE" exec -T nginx \
       nginx -t -c /etc/nginx/options/nginx.conf; then
  echo "nginx config validation failed" >&2
  rollback
  exit 1
fi
docker compose -f "$COMPOSE_FILE" exec -T nginx nginx -s reload

case "$ENV" in staging) NGINX_PORT=9020 ;; prod) NGINX_PORT=9030 ;; esac

for svc in "${PLANNED[@]}"; do
  if ! path=$(health_path_for "$svc"); then
    continue
  fi
  URL="http://localhost:$NGINX_PORT$path"
  attempts=$(health_attempts_for "$svc")
  echo "waiting for $svc /health via nginx: $URL ..."
  healthy=0
  for i in $(seq 1 "$attempts"); do
    if curl -fsS "$URL" >/dev/null 2>&1; then
      healthy=1
      break
    fi
    sleep 2
  done
  if [ "$healthy" -ne 1 ]; then
    echo "$svc health check failed" >&2
    rollback
    exit 1
  fi
done

echo "deploy ok ($ENV @ $IMAGE_TAG -> ${PLANNED[*]})"
