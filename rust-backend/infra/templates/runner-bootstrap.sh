#!/usr/bin/env bash
# Installs docker, awscli, the GH Actions runner tarball, and starts the
# systemd unit that loops ephemeral register → run → exit → respawn.
# Idempotent: safe to re-run; skips work it has already done.
set -euxo pipefail
# shellcheck disable=SC1091
source /etc/default/gh-runner

export DEBIAN_FRONTEND=noninteractive

# ---- Base packages ---------------------------------------------------------
apt-get update
apt-get install -y --no-install-recommends \
  ca-certificates curl gnupg lsb-release jq git unzip sudo

# ---- Docker (ARM) ----------------------------------------------------------
if ! command -v docker >/dev/null; then
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
    | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
  chmod a+r /etc/apt/keyrings/docker.gpg
  echo "deb [arch=arm64 signed-by=/etc/apt/keyrings/docker.gpg] https://download.docker.com/linux/ubuntu $(lsb_release -cs) stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin
fi

# ---- AWS CLI v2 (arm64) ----------------------------------------------------
if ! command -v aws >/dev/null; then
  curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-aarch64.zip" -o /tmp/awscli.zip
  unzip -q /tmp/awscli.zip -d /tmp
  /tmp/aws/install
  rm -rf /tmp/aws /tmp/awscli.zip
fi

# ---- Runner user with docker socket access ---------------------------------
id -u gh-runner >/dev/null 2>&1 || useradd -m -s /bin/bash gh-runner
usermod -aG docker gh-runner

# ---- GitHub Actions runner tarball -----------------------------------------
RUNNER_DIR=/home/gh-runner/actions-runner
mkdir -p "$RUNNER_DIR"
cd "$RUNNER_DIR"

if [ ! -x run.sh ]; then
  curl -fsSL -o runner.tar.gz \
    "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-arm64-${RUNNER_VERSION}.tar.gz"
  tar xzf runner.tar.gz
  rm runner.tar.gz
  # Runner ships a script that installs its OS-level deps (libicu, etc.).
  ./bin/installdependencies.sh
fi
chown -R gh-runner:gh-runner /home/gh-runner

# ---- Start the runner ------------------------------------------------------
systemctl daemon-reload
systemctl enable --now gh-runner.service

echo "runner bootstrap: complete"
