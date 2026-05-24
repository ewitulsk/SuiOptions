#!/usr/bin/env bash
#
# Selective deploy entry point. Pulls images for and rolls only the
# services named in the SERVICES_JSON argument, leaving the rest of the
# env's compose stack untouched.
#
# Invoked by GitHub Actions via `aws ssm send-command`:
#
#   deploy.sh <dev|staging|prod> '["indexer","mm-bot"]'
#
# - IMAGE_TAG is the tag the planned services move to (one tag per
#   deploy; per-service rollback uses the workflow's image_tag dispatch
#   input on a force_all run).
# - Services already declared in this env's compose file but NOT in the
#   request keep their currently-deployed tag — .env carries one
#   <SERVICE>_TAG line per declared service across deploys.
# - Services in the request but NOT declared in this env's compose file
#   are logged + skipped (e.g. asking for mm-bot in prod today).
# - Health-check + rollback runs only when quoting-service is in the
#   planned set (only service exposing /health today).

set -euo pipefail

ENV="${1:?usage: deploy.sh <dev|staging|prod> <services-json>}"
SERVICES_JSON="${2:?services-json argument required, e.g. '[\"indexer\"]'}"

case "$ENV" in
  dev|staging|prod) ;;
  *) echo "unknown env: $ENV" >&2; exit 1 ;;
esac

cd "/opt/options/$ENV"

: "${IMAGE_TAG:?IMAGE_TAG must be set}"
: "${ECR:?ECR must be set}"
: "${DB_HOST:?DB_HOST must be set}"
: "${AWS_REGION:?AWS_REGION must be set}"

COMPOSE_FILE="docker-compose.${ENV}.yml"

# Canonical service set + their .env tag-variable names + the compose
# service name (mostly identical to the cargo crate name, except
# quoting-service is referenced as `quoting` in compose).
ALL_SERVICES=(indexer quoting-service mm-bot option-scheduler api-service)

tag_var_for() {
  case "$1" in
    indexer)          echo INDEXER_TAG ;;
    quoting-service)  echo QUOTING_TAG ;;
    mm-bot)           echo MM_BOT_TAG ;;
    option-scheduler) echo SCHEDULER_TAG ;;
    api-service)      echo API_SERVICE_TAG ;;
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

# Health-check quoting if it was rolled. Other services don't expose
# /health yet — if they're added in the future, extend this loop.
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

for svc in "${PLANNED[@]}"; do
  if [ "$svc" != "quoting-service" ]; then
    continue
  fi
  case "$ENV" in dev) PORT=9012 ;; staging) PORT=9022 ;; prod) PORT=9032 ;; esac
  echo "waiting for quoting /health on :$PORT ..."
  healthy=0
  for i in $(seq 1 30); do
    if curl -fsS "http://localhost:$PORT/health" >/dev/null 2>&1; then
      healthy=1
      break
    fi
    sleep 2
  done
  if [ "$healthy" -ne 1 ]; then
    echo "quoting-service health check failed" >&2
    rollback
    exit 1
  fi
done

echo "deploy ok ($ENV @ $IMAGE_TAG -> ${PLANNED[*]})"
