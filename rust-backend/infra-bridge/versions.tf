# Isolated Terraform root for the bridge signer's Nitro enclave host
# (bridge_tickets/07 Phase 5). Deliberately SEPARATE from rust-backend/infra:
#   - its own (local) state — no shared state with the main root, which carries a
#     known destructive-drift landmine (its ecr.tf for_each `state rm` warnings)
#   - different arch/OS (arm64 Graviton + Nitro vs amd64 Ubuntu)
# So `terraform apply` here never has to `-target` around the main root.
#
# NOTE: local state (matching the repo convention). A remote S3 backend is
# preferable for a shared team; add a `backend "s3"` block here when ready.

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.70"
    }
  }
}

provider "aws" {
  region = var.aws_region
  default_tags {
    tags = {
      Project   = "options"
      ManagedBy = "terraform"
      Component = "bridge-enclave"
    }
  }
}
