#!/usr/bin/env bash
#
# One-time setup on a fresh Ubuntu 22.04 DigitalOcean droplet.
# Idempotent — re-running is safe. Rendered into user_data by infra-do
# with ROLE set to "staging" or "data-room".
#
# Replaces deployment/ec2/ec2-bootstrap.sh: no AWS CLI, no SSM agent.
# Remote access is SSH (deploy key baked in via droplet ssh_keys).

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

# ---- sops + age (secrets decryption on-host) --------------------------------
if ! command -v sops >/dev/null 2>&1; then
  curl -fsSL -o /usr/local/bin/sops \
    "https://github.com/getsops/sops/releases/download/v3.9.4/sops-v3.9.4.linux.amd64"
  chmod 755 /usr/local/bin/sops
fi
if ! command -v age >/dev/null 2>&1; then
  curl -fsSL "https://github.com/FiloSottile/age/releases/download/v1.2.1/age-v1.2.1-linux-amd64.tar.gz" \
    | tar -xz -C /tmp
  install -m 755 /tmp/age/age /tmp/age/age-keygen /usr/local/bin/
  rm -rf /tmp/age
fi

if [ "${ROLE:-staging}" = "staging" ]; then
  # ---- Edge ingress: host nginx + certbot (replaces the ALB + ACM) ----------
  apt-get install -y nginx certbot python3-certbot-nginx

  # ---- Directory layout per env --------------------------------------------
  for env in staging prod; do
    mkdir -p "/opt/options/$env/secrets"
    chmod 700 "/opt/options/$env/secrets"
  done
  mkdir -p /opt/options/monitoring
else
  mkdir -p /opt/data-room /var/spool/data-room
fi

echo "do-host-bootstrap: done (role=${ROLE:-staging})"
