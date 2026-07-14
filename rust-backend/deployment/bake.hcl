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

target "gas-station" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.gas-station"
  tags       = ["${ECR}/options/gas-station:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "gas-station" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "gas-station" }]
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

target "solana-indexer" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-indexer"
  tags       = ["${ECR}/options/solana-indexer:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-indexer" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-indexer" }]
}

# Standalone Solana workspaces (gas-station / keeper / option-scheduler /
# mm-bot / balance-monitor) path-depend on ../solana-contracts/programs, so
# their build context is the REPO ROOT, not rust-backend/. Paths here
# resolve from the `docker buildx bake` working directory (rust-backend/ —
# see _deploy.yml), hence context ".." and dockerfile paths prefixed with
# rust-backend/ (dockerfile resolves relative to the context). The
# repo-root .dockerignore keeps the context to rust-backend/ +
# solana-contracts/.
target "_solana-standalone" {
  context   = ".."
  platforms = ["linux/amd64"]
}

target "solana-token-info" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-token-info"
  tags       = ["${ECR}/options/solana-token-info:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-token-info" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-token-info" }]
}

target "solana-auth-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-auth-service"
  tags       = ["${ECR}/options/solana-auth-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-auth-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-auth-service" }]
}

target "solana-api-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-api-service"
  tags       = ["${ECR}/options/solana-api-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-api-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-api-service" }]
}

target "solana-quoting-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-quoting-service"
  tags       = ["${ECR}/options/solana-quoting-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-quoting-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-quoting-service" }]
}

target "solana-oracle-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-oracle-service"
  tags       = ["${ECR}/options/solana-oracle-service:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-oracle-service" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-oracle-service" }]
}

target "solana-price-charting" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.solana-price-charting"
  tags       = ["${ECR}/options/solana-price-charting:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-price-charting" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-price-charting" }]
}

target "solana-gas-station" {
  inherits   = ["_solana-standalone"]
  dockerfile = "rust-backend/Dockerfile.solana-gas-station"
  tags       = ["${ECR}/options/solana-gas-station:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-gas-station" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-gas-station" }]
}

target "solana-keeper" {
  inherits   = ["_solana-standalone"]
  dockerfile = "rust-backend/Dockerfile.solana-keeper"
  tags       = ["${ECR}/options/solana-keeper:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-keeper" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-keeper" }]
}

target "solana-option-scheduler" {
  inherits   = ["_solana-standalone"]
  dockerfile = "rust-backend/Dockerfile.solana-option-scheduler"
  tags       = ["${ECR}/options/solana-option-scheduler:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-option-scheduler" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-option-scheduler" }]
}

target "solana-mm-bot" {
  inherits   = ["_solana-standalone"]
  dockerfile = "rust-backend/Dockerfile.solana-mm-bot"
  tags       = ["${ECR}/options/solana-mm-bot:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-mm-bot" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-mm-bot" }]
}

target "solana-balance-monitor" {
  inherits   = ["_solana-standalone"]
  dockerfile = "rust-backend/Dockerfile.solana-balance-monitor"
  tags       = ["${ECR}/options/solana-balance-monitor:${IMAGE_TAG}"]
  cache-from = [{ type = "gha", scope = "solana-balance-monitor" }]
  cache-to   = [{ type = "gha", mode = "max", scope = "solana-balance-monitor" }]
}

group "default" {
  targets = ["indexer", "quoting-service", "mm-bot", "option-scheduler", "api-service", "token-info", "auth-service", "gas-station", "price-charting", "keeper", "balance-monitor", "oracle-service", "solana-indexer", "solana-token-info", "solana-auth-service", "solana-api-service", "solana-quoting-service", "solana-oracle-service", "solana-price-charting", "solana-gas-station", "solana-keeper", "solana-option-scheduler", "solana-mm-bot", "solana-balance-monitor"]
}
