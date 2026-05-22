# `docker buildx bake` config for the three service images.
# Targets linux/arm64 (Graviton EC2). On x86 GH runners the build goes
# through QEMU emulation; ~2x slower than native, still under 10 minutes.

variable "ECR" {
  default = ""
}

variable "IMAGE_TAG" {
  default = "dev"
}

group "default" {
  targets = ["indexer", "quoting-service", "mm-bot"]
}

target "_common" {
  context    = "."
  platforms  = ["linux/arm64"]
  cache-from = [{ type = "gha" }]
  cache-to   = [{ type = "gha", mode = "max" }]
}

target "indexer" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.indexer"
  tags       = ["${ECR}/options/indexer:${IMAGE_TAG}"]
}

target "quoting-service" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.quoting"
  tags       = ["${ECR}/options/quoting-service:${IMAGE_TAG}"]
}

target "mm-bot" {
  inherits   = ["_common"]
  dockerfile = "Dockerfile.mm-bot"
  tags       = ["${ECR}/options/mm-bot:${IMAGE_TAG}"]
}
