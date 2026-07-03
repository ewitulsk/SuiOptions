variable "aws_region" {
  description = "AWS region. Match the main infra so we share its VPC."
  type        = string
  default     = "us-east-1"
}

variable "project" {
  description = "Resource-name prefix. Also used to look up the existing VPC/subnet by tag."
  type        = string
  default     = "options"
}

variable "instance_type" {
  description = "Enclave host. c7g.large = 2 vCPU / 4 GB Graviton — the smallest Nitro-Enclave-capable size (no SMT, so the 2-vCPU floor applies; Intel/AMD would need 4). Burstable (t*) and single-core are excluded from Nitro Enclaves."
  type        = string
  default     = "c7g.large"
}

variable "root_volume_gb" {
  description = "Root EBS (gp3). Holds the OS + docker + the EIF."
  type        = number
  default     = 30
}

variable "enclave_cpu_count" {
  description = "vCPUs dedicated to the enclave. On c7g.large (2 vCPU) use 1, leaving 1 for the parent + vsock proxy."
  type        = number
  default     = 1
}

variable "enclave_memory_mib" {
  description = "Memory dedicated to the enclave. The signer (axum + rustls + verifier) is light; 1536 leaves ~2.5 GB for the parent."
  type        = number
  default     = 1536
}

variable "ecr_repo_name" {
  description = "ECR repo for the enclave app image. MUST match the CI var BRIDGE_ENCLAVE_ECR_REPO used by .github/workflows/bridge-enclave.yml."
  type        = string
  default     = "options-bridge-signer-enclave"
}

variable "signer_api_ingress_cidrs" {
  description = "CIDRs allowed to reach the signer public API (tcp/3000) — e.g. the relayer host. Empty = no inbound (host is SSM-managed; no port needed until the signer serves)."
  type        = list(string)
  default     = []
}
