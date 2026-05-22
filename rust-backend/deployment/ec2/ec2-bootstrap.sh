#!/usr/bin/env bash
#
# One-time setup on a fresh Ubuntu 22.04 ARM EC2 instance.
# Idempotent — re-running is safe.
#
# Terraform's `user_data` runs this on first boot; you can also re-run it
# by hand via SSM Session Manager if you need to.

set -euo pipefail

export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends \
  ca-certificates curl gnupg unzip jq

# ---- Docker -----------------------------------------------------------------
if ! command -v docker >/dev/null 2>&1; then
  install -m 0755 -d /etc/apt/keyrings
  curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
    | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
  chmod a+r /etc/apt/keyrings/docker.gpg
  ARCH=$(dpkg --print-architecture)
  CODENAME=$(. /etc/os-release && echo "$VERSION_CODENAME")
  echo "deb [arch=$ARCH signed-by=/etc/apt/keyrings/docker.gpg] \
    https://download.docker.com/linux/ubuntu $CODENAME stable" \
    > /etc/apt/sources.list.d/docker.list
  apt-get update
  apt-get install -y docker-ce docker-ce-cli containerd.io \
                     docker-buildx-plugin docker-compose-plugin
  systemctl enable --now docker
fi

# ---- AWS CLI v2 -------------------------------------------------------------
if ! command -v aws >/dev/null 2>&1; then
  curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-aarch64.zip" \
    -o /tmp/awscliv2.zip
  unzip -q /tmp/awscliv2.zip -d /tmp
  /tmp/aws/install
  rm -rf /tmp/aws /tmp/awscliv2.zip
fi

# ---- SSM Agent (already on Amazon Linux; Ubuntu needs snap) -----------------
if ! systemctl is-active --quiet amazon-ssm-agent 2>/dev/null; then
  snap install amazon-ssm-agent --classic || true
  systemctl enable --now snap.amazon-ssm-agent.amazon-ssm-agent.service || true
fi

# ---- Directory layout per env ----------------------------------------------
for env in dev staging prod; do
  mkdir -p "/opt/options/$env/secrets"
  chmod 700 "/opt/options/$env/secrets"
done

echo "ec2-bootstrap: done"
