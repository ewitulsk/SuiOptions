# `docker buildx bake` config for the workspace's service images.
# Targets linux/amd64 (t3.* Intel EC2). Builds native on x86 GH runners
# — no QEMU emulation. Switching to ARM means: bump platforms below,
# add setup-qemu-action back to _deploy.yml, swap the EC2 instance
# family + AMI filter in infra/ to t4g.* + arm64-server.
#
# Typical invocation (CI): `docker buildx bake --push <target>` where
# <target> is one of indexer / quoting-service / mm-bot / option-scheduler.
# The selective-deploy workflow names exactly the affected targets; the
# `default` group below is the "build everything" fallback for
# workflow_dispatch with `force_all=true` and for local one-off builds.

variable "ECR" {
  default = ""
}

variable "IMAGE_TAG" {
  default = "local"
}

# Cache layer separation per service: each target writes its own gha
# cache scope so a change to one service's source doesn't invalidate
# another's compiled layers. The scope strings must be stable across
# runs — keep them in sync with the target names.
target "_common" {
  context   = "."
  platforms = ["linux/amd64"]
}

target "indexer" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.indexer"
  tags       = ["${ECR}/options/indexer:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "indexer" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "indexer" }]
}

target "quoting-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.quoting"
  tags       = ["${ECR}/options/quoting-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "quoting-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "quoting-service" }]
}

target "mm-bot" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.mm-bot"
  tags       = ["${ECR}/options/mm-bot:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "mm-bot" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "mm-bot" }]
}

target "option-scheduler" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.scheduler"
  tags       = ["${ECR}/options/option-scheduler:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "option-scheduler" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "option-scheduler" }]
}

target "api-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.api-service"
  tags       = ["${ECR}/options/api-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "api-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "api-service" }]
}

target "token-info" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.token-info"
  tags       = ["${ECR}/options/token-info:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "token-info" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "token-info" }]
}

target "auth-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.auth-service"
  tags       = ["${ECR}/options/auth-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "auth-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "auth-service" }]
}

target "price-charting" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.price-charting"
  tags       = ["${ECR}/options/price-charting:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "price-charting" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "price-charting" }]
}

target "cctp-relay" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.cctp-relay"
  tags       = ["${ECR}/options/cctp-relay:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "cctp-relay" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "cctp-relay" }]
}

target "gas-station" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.gas-station"
  tags       = ["${ECR}/options/gas-station:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "gas-station" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "gas-station" }]
}

target "hedge-signer" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.hedge-signer"
  tags       = ["${ECR}/options/hedge-signer:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "hedge-signer" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "hedge-signer" }]
}

target "keeper" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.keeper"
  tags       = ["${ECR}/options/keeper:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "keeper" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "keeper" }]
}

target "balance-monitor" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.balance-monitor"
  tags       = ["${ECR}/options/balance-monitor:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "balance-monitor" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "balance-monitor" }]
}

target "oracle-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.oracle-service"
  tags       = ["${ECR}/options/oracle-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "oracle-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "oracle-service" }]
}

target "twitter-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.twitter-service"
  tags       = ["${ECR}/options/twitter-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "twitter-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "twitter-service" }]
}

target "social-bot" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.social-bot"
  tags       = ["${ECR}/options/social-bot:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "social-bot" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "social-bot" }]
}

group "default" {
  targets = ["indexer", "quoting-service", "mm-bot", "option-scheduler", "api-service", "token-info", "auth-service", "gas-station", "hedge-signer", "price-charting", "keeper", "balance-monitor", "oracle-service", "cctp-relay", "twitter-service", "social-bot"]
}
