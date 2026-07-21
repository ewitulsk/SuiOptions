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
    compose_staging        = file("${path.module}/../deployment/compose/docker-compose.staging.yml")
    compose_prod           = file("${path.module}/../deployment/compose/docker-compose.prod.yml")
    promtail_config        = file("${path.module}/../deployment/monitoring/promtail-config.yml")
    promtail_compose       = file("${path.module}/../deployment/monitoring/docker-compose.promtail.yml")
    loki_config            = file("${path.module}/../deployment/monitoring/loki-config.yml")
    prometheus_config      = file("${path.module}/../deployment/monitoring/prometheus.yml")
    tempo_config           = file("${path.module}/../deployment/monitoring/tempo-config.yml")
    monitoring_compose     = file("${path.module}/../deployment/monitoring/docker-compose.monitoring.yml")
    grafana_ds             = file("${path.module}/../deployment/monitoring/grafana-datasources.yml")
    grafana_alerting       = file("${path.module}/../deployment/monitoring/grafana-alerting.yml")
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
  ami                         = var.host_ami
  instance_type               = var.staging_ec2_instance_type
  subnet_id                   = aws_subnet.public[0].id
  vpc_security_group_ids      = [aws_security_group.ec2.id]
  iam_instance_profile        = aws_iam_instance_profile.ec2.name
  associate_public_ip_address = true
  key_name                    = var.ssh_pubkey == "" ? null : aws_key_pair.host[0].key_name

  # gzip+base64 to keep the first-boot payload small (cloud-init
  # transparently decompresses gzipped user_data). NOTE: the gzipped
  # staging payload has crept past EC2's hard 16 KB user_data cap, so a
  # ModifyInstanceAttribute that includes user_data now fails. cloud-init
  # only runs user_data once at first boot, so pushing it to a running
  # instance is a no-op anyway — ignore_changes below stops terraform from
  # attempting it on every apply. Bootstrap changes still require a
  # deliberate instance replacement (user_data_replace_on_change = false).
  user_data_base64            = base64gzip(local.bootstrap_user_data)
  user_data_replace_on_change = false

  root_block_device {
    volume_size           = var.staging_ec2_root_volume_gb
    volume_type           = "gp3"
    delete_on_termination = true
    encrypted             = true
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required" # IMDSv2 only
    http_put_response_hop_limit = 2          # docker reaches the IMDS
  }

  lifecycle {
    ignore_changes = [user_data_base64]
  }

  tags = {
    Name = "${var.project}-host"
  }
}

# ---- Dedicated prod host ----------------------------------------------------
# prod runs on its own instance (var.ec2_instance_type, t3a.medium), sized to
# match the shared staging host (var.staging_ec2_instance_type). It
# runs ONLY the prod compose stack plus a Promtail shipper that forwards
# container logs to the central Loki on the shared host over the private
# VPC IP. No Loki/Grafana/Gatus server and no Tailscale router live here —
# those stay on the shared host. Reuses the shared SG + instance profile,
# so RDS access and ALB ingress (9030) are already permitted.

locals {
  prod_bootstrap_user_data = templatefile("${path.module}/templates/cloud-init.prod.sh.tftpl", {
    bootstrap_script  = file("${path.module}/../deployment/ec2/ec2-bootstrap.sh")
    deploy_script     = file("${path.module}/../deployment/ec2/deploy.sh")
    render_secrets_sh = file("${path.module}/../deployment/ec2/render-secrets.sh")
    compose_prod      = file("${path.module}/../deployment/compose/docker-compose.prod.yml")
    promtail_config   = file("${path.module}/../deployment/monitoring/promtail-config.yml")
    promtail_compose  = file("${path.module}/../deployment/monitoring/docker-compose.promtail.yml")
    prom_agent_config = file("${path.module}/../deployment/monitoring/prometheus-agent.yml")
    prom_agent_compose = file("${path.module}/../deployment/monitoring/docker-compose.prom-agent.yml")
    aws_region        = var.aws_region
    # Ship logs to the central Loki on the shared host, reachable over the
    # private VPC IP (SG self-ingress on 3100, see security_groups.tf).
    # Metrics and traces follow the same pattern (SO-180): the prom agent
    # remote-writes to 9090, the services push OTLP spans to Tempo on 4318.
    loki_url         = "http://${aws_instance.host.private_ip}:3100"
    remote_write_url = "http://${aws_instance.host.private_ip}:9090/api/v1/write"
    otel_url         = "http://${aws_instance.host.private_ip}:4318"
  })
}

resource "aws_instance" "prod_host" {
  ami                         = var.host_ami
  instance_type               = var.ec2_instance_type
  subnet_id                   = aws_subnet.public[0].id
  vpc_security_group_ids      = [aws_security_group.ec2.id]
  iam_instance_profile        = aws_iam_instance_profile.ec2.name
  associate_public_ip_address = true
  key_name                    = var.ssh_pubkey == "" ? null : aws_key_pair.host[0].key_name

  user_data_base64            = base64gzip(local.prod_bootstrap_user_data)
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

  # Same rationale as the staging host: cloud-init runs user_data once at
  # first boot, so terraform should not try to push it to a running
  # instance. prod's payload is still under 16 KB today, but ignore here too
  # for consistency and to avoid the same failure as it grows.
  lifecycle {
    ignore_changes = [user_data_base64]
  }

  tags = {
    Name = "${var.project}-prod-host"
  }
}
