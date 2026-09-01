terraform {
  required_version = ">= 1.6"

  # Remote state in DO Spaces (S3-compatible). Credentials come from
  # AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY env vars set to a Spaces key
  # at init/plan/apply time — never AWS credentials.
  backend "s3" {
    bucket                      = "suioptions-tfstate"
    key                         = "infra-do/terraform.tfstate"
    region                      = "us-east-1" # ignored by Spaces; required by the backend
    endpoints                   = { s3 = "https://nyc3.digitaloceanspaces.com" }
    skip_credentials_validation = true
    skip_requesting_account_id  = true
    skip_metadata_api_check     = true
    skip_region_validation      = true
    skip_s3_checksum            = true
  }

  required_providers {
    digitalocean = {
      source  = "digitalocean/digitalocean"
      version = "~> 2.50"
    }
  }
}

# Token from DIGITALOCEAN_TOKEN env var.
provider "digitalocean" {}
