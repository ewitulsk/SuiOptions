#!/usr/bin/env bash
# Runs as ExecStartPre on every gh-runner.service start. Pulls the PAT,
# mints a fresh registration token from the GH API, and registers the
# runner in --ephemeral mode (auto-deregisters after one job).
set -euo pipefail
# shellcheck disable=SC1091
source /etc/default/gh-runner

PAT=$(aws secretsmanager get-secret-value \
  --region "$AWS_REGION" \
  --secret-id "$PAT_SECRET_ARN" \
  --query SecretString --output text | jq -r .pat)

if [ "$PAT" = "REPLACE_ME" ] || [ -z "$PAT" ] || [ "$PAT" = "null" ]; then
  echo "ERROR: $PAT_SECRET_ARN still contains placeholder. Populate it before the runner can register." >&2
  exit 1
fi

TOKEN=$(curl -fsSL -X POST \
  -H "Authorization: Bearer $PAT" \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "https://api.github.com/repos/$GITHUB_REPO/actions/runners/registration-token" \
  | jq -r .token)

cd /home/gh-runner/actions-runner

# --replace handles stale entries left over from an ungraceful exit
# (spot reclaim, OOM, etc.) where the prior runner couldn't deregister.
sudo -u gh-runner ./config.sh \
  --unattended \
  --ephemeral \
  --url "https://github.com/$GITHUB_REPO" \
  --token "$TOKEN" \
  --name "$(hostname)" \
  --labels "$RUNNER_LABELS" \
  --work _work \
  --replace
