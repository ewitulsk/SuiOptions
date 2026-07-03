resource "aws_instance" "enclave" {
  ami                         = data.aws_ssm_parameter.al2023_arm64.value
  instance_type               = var.instance_type
  subnet_id                   = data.aws_subnet.public.id
  vpc_security_group_ids      = [aws_security_group.enclave.id]
  iam_instance_profile        = aws_iam_instance_profile.enclave.name
  associate_public_ip_address = true # public subnet: egress for SSM/ECR/RPC without a NAT

  # The whole point: enable Nitro Enclaves on this host.
  enclave_options {
    enabled = true
  }

  # IMDSv2 only.
  metadata_options {
    http_endpoint = "enabled"
    http_tokens   = "required"
  }

  root_block_device {
    volume_type = "gp3"
    volume_size = var.root_volume_gb
    encrypted   = true
  }

  user_data = templatefile("${path.module}/templates/user_data.sh.tftpl", {
    enclave_cpu_count  = var.enclave_cpu_count
    enclave_memory_mib = var.enclave_memory_mib
  })

  tags = { Name = "${var.project}-bridge-enclave" }

  # A new AL2023 AMI release must not silently force-replace the running enclave
  # host. Upgrade deliberately (taint + apply) when desired.
  lifecycle {
    ignore_changes = [ami]
  }
}
