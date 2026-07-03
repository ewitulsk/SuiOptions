resource "aws_security_group" "enclave" {
  name_prefix = "${var.project}-bridge-enclave-"
  description = "Bridge signer enclave host"
  vpc_id      = data.aws_vpc.main.id

  # Egress: allow all — SSM, ECR, RPC providers, and Seal servers are all HTTPS
  # to varied endpoints. Tighten to 443 + DNS(53) once the exact set is fixed.
  egress {
    description = "all outbound (SSM/ECR/RPC/Seal)"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  # Ingress: signer public API (tcp/3000) only if configured (e.g. the relayer).
  # Default empty → no inbound; the host is managed via SSM, not SSH.
  dynamic "ingress" {
    for_each = length(var.signer_api_ingress_cidrs) > 0 ? [1] : []
    content {
      description = "signer public API (/sign_requests)"
      from_port   = 3000
      to_port     = 3000
      protocol    = "tcp"
      cidr_blocks = var.signer_api_ingress_cidrs
    }
  }

  tags = { Name = "${var.project}-bridge-enclave" }

  lifecycle {
    create_before_destroy = true
  }
}
