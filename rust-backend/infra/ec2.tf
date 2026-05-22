# Ubuntu 22.04 LTS ARM64 (the cheapest x86-equivalent in the t4g family).
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-arm64-server-*"]
  }
  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

resource "aws_key_pair" "host" {
  count      = var.ssh_pubkey == "" ? 0 : 1
  key_name   = "${var.project}-ec2"
  public_key = var.ssh_pubkey
}

# Bootstrap script that runs once on first boot. Installs docker, AWS CLI,
# SSM Agent, lays out /opt/options/<env>, and drops the compose + deploy
# files into each env dir. The actual content of those files comes from
# the repo (we render them at apply time via templatefile).

locals {
  bootstrap_user_data = templatefile("${path.module}/templates/cloud-init.sh.tftpl", {
    bootstrap_script    = file("${path.module}/../deployment/ec2/ec2-bootstrap.sh")
    deploy_script       = file("${path.module}/../deployment/ec2/deploy.sh")
    render_secrets_sh   = file("${path.module}/../deployment/ec2/render-secrets.sh")
    compose_dev         = file("${path.module}/../deployment/compose/docker-compose.dev.yml")
    compose_staging     = file("${path.module}/../deployment/compose/docker-compose.staging.yml")
    compose_prod        = file("${path.module}/../deployment/compose/docker-compose.prod.yml")
  })
}

resource "aws_instance" "host" {
  ami                         = data.aws_ami.ubuntu.id
  instance_type               = var.ec2_instance_type
  subnet_id                   = aws_subnet.public[0].id
  vpc_security_group_ids      = [aws_security_group.ec2.id]
  iam_instance_profile        = aws_iam_instance_profile.ec2.name
  associate_public_ip_address = true
  key_name                    = var.ssh_pubkey == "" ? null : aws_key_pair.host[0].key_name

  user_data                   = local.bootstrap_user_data
  user_data_replace_on_change = false

  root_block_device {
    volume_size           = var.ec2_root_volume_gb
    volume_type           = "gp3"
    delete_on_termination = true
    encrypted             = true
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required" # IMDSv2 only
    http_put_response_hop_limit = 2          # docker reaches the IMDS
  }

  tags = {
    Name = "${var.project}-host"
  }
}
