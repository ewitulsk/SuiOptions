# Self-hosted GH Actions runner on a spot instance. Builds the three
# service images natively on ARM (no QEMU cross-compile), then exits;
# deploys still go through GH-hosted runners + OIDC + SSM (see workflows).
#
# Spot type "persistent" + interruption behavior "stop" means AWS keeps
# the spot request alive and the root EBS persists across reclaims —
# when capacity returns, the same instance starts back up and systemd
# auto-relaunches the runner.

locals {
  runner_user_data = templatefile("${path.module}/templates/runner-cloud-init.sh.tftpl", {
    github_repo       = var.github_repo
    runner_labels     = "self-hosted,linux,arm64,options-ci"
    pat_secret_arn    = aws_secretsmanager_secret.github_runner_pat.arn
    aws_region        = var.aws_region
    runner_version    = var.runner_version
    bootstrap_script  = file("${path.module}/templates/runner-bootstrap.sh")
    register_script   = file("${path.module}/templates/runner-register.sh")
    systemd_unit      = file("${path.module}/templates/runner.service")
  })
}

resource "aws_spot_instance_request" "runner" {
  ami                            = data.aws_ami.ubuntu.id
  instance_type                  = var.runner_instance_type
  subnet_id                      = aws_subnet.public[0].id
  vpc_security_group_ids         = [aws_security_group.runner.id]
  iam_instance_profile           = aws_iam_instance_profile.runner.name
  associate_public_ip_address    = true
  key_name                       = var.ssh_pubkey == "" ? null : aws_key_pair.host[0].key_name

  spot_type                      = "persistent"
  wait_for_fulfillment           = true
  instance_interruption_behavior = "stop"
  spot_price                     = var.runner_max_spot_price == "" ? null : var.runner_max_spot_price

  user_data                   = local.runner_user_data
  user_data_replace_on_change = false

  root_block_device {
    volume_size           = var.runner_root_volume_gb
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
    Name = "${var.project}-runner-spot-request"
  }
}

# Tags on the spot request don't propagate to the underlying instance;
# tag it directly so it's discoverable in the EC2 console.
resource "aws_ec2_tag" "runner_name" {
  resource_id = aws_spot_instance_request.runner.spot_instance_id
  key         = "Name"
  value       = "${var.project}-runner"
}
