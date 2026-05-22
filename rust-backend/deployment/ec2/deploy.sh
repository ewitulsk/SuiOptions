#!/usr/bin/env bash
#
# Pull the latest images, restart the env's compose stack, health-check
# the quoting-service, roll back if anything's wrong.
#
# Invoked by GitHub Actions via `aws ssm send-command`. The image tag to
# deploy is read from the IMAGE_TAG env var on the way in.

set -euo pipefail

ENV="${1:?usage: deploy.sh <dev|staging|prod>}"
case "$ENV" in
  dev)     PORT=9012 ;;
  staging) PORT=9022 ;;
  prod)    PORT=9032 ;;
  *) echo "unknown env: $ENV" >&2; exit 1 ;;
esac

cd "/opt/options/$ENV"

: "${IMAGE_TAG:?IMAGE_TAG must be set}"
: "${ECR:?ECR must be set}"
: "${DB_HOST:?DB_HOST must be set}"
: "${AWS_REGION:?AWS_REGION must be set}"

COMPOSE_FILE="docker-compose.${ENV}.yml"
PREV_TAG=""
if [ -f .env ]; then
  PREV_TAG=$(grep '^IMAGE_TAG=' .env | cut -d= -f2 || echo "")
fi

# Fetch secrets + write .env atomically.
./render-secrets.sh "$ENV"
DB_PASSWORD=$(cat "secrets/.db_password")

# Atomic .env swap.
cat > .env.new <<EOF
ECR=$ECR
IMAGE_TAG=$IMAGE_TAG
DB_PASSWORD=$DB_PASSWORD
DB_HOST=$DB_HOST
EOF
mv .env.new .env

# ECR auth, pull, up.
aws ecr get-login-password --region "$AWS_REGION" \
  | docker login --username AWS --password-stdin "$ECR"

docker compose -f "$COMPOSE_FILE" pull
docker compose -f "$COMPOSE_FILE" up -d --remove-orphans

# Health check the quoting-service. 30s grace for cold starts.
echo "waiting for quoting-service /health on :$PORT ..."
healthy=0
for i in $(seq 1 30); do
  if curl -fsS "http://localhost:$PORT/health" >/dev/null 2>&1; then
    healthy=1
    break
  fi
  sleep 2
done

if [ "$healthy" -ne 1 ]; then
  echo "health check failed; rolling back" >&2
  if [ -n "$PREV_TAG" ] && [ "$PREV_TAG" != "$IMAGE_TAG" ]; then
    sed -i "s/^IMAGE_TAG=.*/IMAGE_TAG=$PREV_TAG/" .env
    docker compose -f "$COMPOSE_FILE" up -d
  else
    echo "no prior tag recorded; leaving current state" >&2
  fi
  exit 1
fi

echo "deploy ok ($ENV @ $IMAGE_TAG)"
