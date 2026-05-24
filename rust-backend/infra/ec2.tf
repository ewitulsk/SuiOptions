# Ubuntu 22.04 LTS amd64. Matches t3.* (Intel). Keep this filter in
# sync with ec2_instance_type — switching the instance family without
# switching the AMI architecture would fail to boot.
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd/ubuntu-jammy-22.04-amd64-server-*"]
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
    bootstrap_script       = file("${path.module}/../deployment/ec2/ec2-bootstrap.sh")
    deploy_script          = file("${path.module}/../deployment/ec2/deploy.sh")
    render_secrets_sh      = file("${path.module}/../deployment/ec2/render-secrets.sh")
    compose_dev            = file("${path.module}/../deployment/compose/docker-compose.dev.yml")
    compose_staging        = file("${path.module}/../deployment/compose/docker-compose.staging.yml")
    compose_prod           = file("${path.module}/../deployment/compose/docker-compose.prod.yml")
    promtail_config        = file("${path.module}/../deployment/monitoring/promtail-config.yml")
    promtail_compose       = file("${path.module}/../deployment/monitoring/docker-compose.promtail.yml")
    loki_config            = file("${path.module}/../deployment/monitoring/loki-config.yml")
    monitoring_compose     = file("${path.module}/../deployment/monitoring/docker-compose.monitoring.yml")
    grafana_ds             = file("${path.module}/../deployment/monitoring/grafana-datasources.yml")
    gatus_config           = file("${path.module}/../deployment/monitoring/gatus-config.yml")
    loki_bucket            = aws_s3_bucket.loki.bucket
    aws_region             = var.aws_region
    grafana_secret         = aws_secretsmanager_secret.grafana_admin.name
    domain_name            = var.domain_name
    vpc_cidr               = var.vpc_cidr
    tailscale_auth_key_arn = aws_secretsmanager_secret.tailscale_auth_key.arn
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

  # gzip+base64 to stay under EC2's 16 KB user_data limit (cloud-init
  # transparently decompresses gzipped payloads).
  user_data_base64            = base64gzip(local.bootstrap_user_data)
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
