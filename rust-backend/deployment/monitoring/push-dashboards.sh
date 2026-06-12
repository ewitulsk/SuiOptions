#!/usr/bin/env bash
#
# Push the repo's Grafana dashboards (deployment/monitoring/dashboards/*.json)
# to a Grafana instance via the HTTP API. Dashboards land in the "Options"
# folder and overwrite previous versions, so this is the dashboard deploy
# path — no cloud-init / host bounce involved.
#
# Usage:
#   GRAFANA_URL=https://<domain>/grafana GRAFANA_PASSWORD=... ./push-dashboards.sh
#
# GRAFANA_USER defaults to admin. The admin password lives in Secrets
# Manager under options/monitoring/grafana-admin.

set -euo pipefail

: "${GRAFANA_URL:?GRAFANA_URL must be set (e.g. https://example.com/grafana)}"
: "${GRAFANA_PASSWORD:?GRAFANA_PASSWORD must be set}"
GRAFANA_USER="${GRAFANA_USER:-admin}"

DIR="$(cd "$(dirname "$0")" && pwd)/dashboards"
AUTH=(-u "$GRAFANA_USER:$GRAFANA_PASSWORD" -H "Content-Type: application/json")

# Ensure the Options folder exists (409 = already there).
FOLDER_UID="options"
curl -fsS "${AUTH[@]}" -X POST "$GRAFANA_URL/api/folders" \
  -d "{\"uid\": \"$FOLDER_UID\", \"title\": \"Options\"}" >/dev/null 2>&1 || true

for f in "$DIR"/*.json; do
  name=$(basename "$f")
  payload=$(jq -n --slurpfile d "$f" --arg folder "$FOLDER_UID" \
    '{dashboard: $d[0], folderUid: $folder, overwrite: true}')
  echo "pushing $name ..."
  curl -fsS "${AUTH[@]}" -X POST "$GRAFANA_URL/api/dashboards/db" \
    -d "$payload" >/dev/null
done

echo "dashboards pushed: $(ls "$DIR" | tr '\n' ' ')"
