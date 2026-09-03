variable "region" {
  type    = string
  default = "nyc3"
}

variable "vpc_name" {
  type    = string
  default = "options-vpc" # created out-of-band 2026-09-01; referenced as a data source
}

variable "staging_droplet_size" {
  type = string
  # Plan called for s-4vcpu-8gb, but nyc3 had no >=8GB capacity at migration
  # time (2026-09-01); s-2vcpu-4gb matches the old t3a.medium and resizes
  # in place (reboot) when bigger sizes return to the region.
  default = "s-2vcpu-4gb"
}

variable "data_room_droplet_size" {
  type    = string
  default = "s-2vcpu-4gb"
}

variable "droplet_image" {
  type    = string
  default = "ubuntu-22-04-x64"
}

variable "data_room_volume_gb" {
  type    = number
  default = 100
}

variable "nft_droplet_size" {
  type = string
  # Cost-sized: s-2vcpu-2gb ($18/mo) instead of the plan's s-2vcpu-4gb
  # ($24/mo). Keeps 2 vCPUs for the stream decoder; 2 GB RAM fits P0
  # (postgres + indexer + api, near-zero traffic). Resize in place if
  # memory pressure appears; backups stay OFF (pg_dump to Spaces instead).
  default = "s-2vcpu-2gb"
}

variable "nft_domain" {
  type    = string
  default = "nft.sui-options.com"
}

variable "deploy_ssh_pubkey" {
  description = "Public key used by GitHub Actions deploys (private half in the DEPLOY_SSH_KEY repo secret)"
  type        = string
}

variable "operator_ssh_pubkey" {
  description = "Evan's personal public key for interactive access"
  type        = string
  default     = ""
}
