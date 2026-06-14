variable "aws_region" {
  description = "AWS region for every resource."
  type        = string
  default     = "us-east-1"
}

variable "project" {
  description = "Short prefix used in every resource name."
  type        = string
  default     = "options"
}

variable "vpc_cidr" {
  description = "Top-level CIDR for the fresh VPC."
  type        = string
  default     = "10.40.0.0/16"
}

variable "public_subnet_cidrs" {
  description = "Two public subnet CIDRs, one per AZ. Used by EC2 + ALB."
  type        = list(string)
  default     = ["10.40.1.0/24", "10.40.2.0/24"]
}

variable "private_subnet_cidrs" {
  description = "Two private subnet CIDRs, one per AZ. Used by RDS (multi-AZ requirement for a DB subnet group even in single-AZ mode)."
  type        = list(string)
  default     = ["10.40.11.0/24", "10.40.12.0/24"]
}

# ---- App ingress ------------------------------------------------------------

variable "domain_name" {
  description = "DNS name the ALB will serve from (e.g. api.options.example.com)."
  type        = string
}

variable "route53_zone_id" {
  description = "Hosted zone ID for the domain. Leave empty to skip Route53 + ACM validation; you'll wire DNS by hand."
  type        = string
  default     = ""
}

# ---- EC2 --------------------------------------------------------------------

variable "host_ami" {
  description = "AMI for the EC2 hosts. Pinned (not a most_recent lookup) so a new Canonical release doesn't force-replace the running instances on every apply. Upgrade deliberately: find the newest Ubuntu 22.04 amd64 image with `aws ec2 describe-images --owners 099720109477 --filters 'Name=name,Values=ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*' --query 'sort_by(Images,&CreationDate)[-1].ImageId'` and set it here. Keep the arch (amd64) in sync with ec2_instance_type."
  type        = string
  default     = "ami-02013f5b15758f4d4"
}

variable "ec2_instance_type" {
  description = "EC2 instance type. x86 (t3.*) so GH-runner builds stay native — no QEMU emulation. Switching to t4g.* (Graviton/ARM) needs a matching change in bake.hcl + the AMI filter in ec2.tf + setup-qemu-action in _deploy.yml."
  type        = string
  default     = "t3.small"
}

variable "ec2_root_volume_gb" {
  description = "EBS gp3 root volume size in GB."
  type        = number
  default     = 30
}

variable "ssh_pubkey" {
  description = "Optional SSH public key (OpenSSH format) to add to the EC2 key pair. Leave empty to disable SSH; SSM Session Manager works regardless."
  type        = string
  default     = ""
}

# ---- RDS --------------------------------------------------------------------

variable "rds_instance_class" {
  description = "RDS instance class."
  type        = string
  default     = "db.t4g.micro"
}

variable "rds_allocated_storage_gb" {
  description = "RDS storage in GB. Postgres on db.t4g.micro caps at 20GiB on free tier; bump for prod-grade growth."
  type        = number
  default     = 20
}

variable "rds_backup_retention_days" {
  description = "Automated backup retention. 7 is the cheap-tier default."
  type        = number
  default     = 7
}

# ---- GitHub Actions OIDC ----------------------------------------------------

variable "github_repo" {
  description = "owner/repo for the GH Actions OIDC trust policy (e.g. evanw/options)."
  type        = string
}

variable "github_deploy_branches" {
  description = "Branches allowed to assume the deploy role."
  type        = list(string)
  default     = ["staging", "main"]
}
