data "aws_caller_identity" "current" {}

# Reuse the main infra's VPC + a public subnet (looked up by the tags the main
# root sets: "<project>-vpc", "<project>-public-0"). We don't recreate networking.
data "aws_vpc" "main" {
  tags = { Name = "${var.project}-vpc" }
}

data "aws_subnet" "public" {
  vpc_id = data.aws_vpc.main.id
  tags   = { Name = "${var.project}-public-0" }
}

# Latest Amazon Linux 2023 arm64 AMI (AL2023 has nitro-cli in dnf + SSM agent
# preinstalled). Pinned against replacement via ignore_changes on the instance.
data "aws_ssm_parameter" "al2023_arm64" {
  name = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64"
}
