# `docker buildx bake` config for the workspace's service images.
# Targets linux/arm64 (Graviton EC2). On x86 GH runners the build goes
# through QEMU emulation; ~2x slower than native, still under 10 minutes.
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
  default = "dev"
}

# Cache layer separation per service: each target writes its own gha
# cache scope so a change to one service's source doesn't invalidate
# another's compiled layers. The scope strings must be stable across
# runs — keep them in sync with the target names.
target "_common" {
  context    = "."
  platforms  = ["linux/arm64"]
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

group "default" {
  targets = ["indexer", "quoting-service", "mm-bot", "option-scheduler"]
}
