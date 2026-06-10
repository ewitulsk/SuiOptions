# Monitoring stack: Grafana + Loki run on the services EC2 alongside the
# per-env compose stacks. Promtail also runs on the same host and ships
# Docker container logs to Loki over localhost.
#
# Persistence guarantees:
#   - Loki chunks + index: S3 bucket with versioning. Terraform will not
#     destroy the bucket on `terraform destroy` (force_destroy = false).
#   - Grafana data (dashboards, users, alert rules): Docker named volume
#     on the services EC2 root EBS.  Survives container restarts and
#     redeployments.  Back up the EBS / snapshot it for disaster recovery.

# ---------------------------------------------------------------------------
# S3 bucket for Loki chunk + index storage
# ---------------------------------------------------------------------------

resource "aws_s3_bucket" "loki" {
  bucket_prefix = "${var.project}-loki-"
  force_destroy = false

  tags = {
    Name = "${var.project}-loki-storage"
  }
}

resource "aws_s3_bucket_versioning" "loki" {
  bucket = aws_s3_bucket.loki.id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "loki" {
  bucket = aws_s3_bucket.loki.id

  rule {
    id     = "expire-old-chunks"
    status = "Enabled"
    filter {}
    expiration {
      days = 90
    }
    noncurrent_version_expiration {
      noncurrent_days = 30
    }
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "loki" {
  bucket = aws_s3_bucket.loki.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "loki" {
  bucket                  = aws_s3_bucket.loki.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# ---------------------------------------------------------------------------
# Grafana admin password in Secrets Manager
# ---------------------------------------------------------------------------

resource "random_password" "grafana_admin" {
  count   = var.grafana_admin_password == "" ? 1 : 0
  length  = 24
  special = false
}

locals {
  grafana_password = var.grafana_admin_password != "" ? var.grafana_admin_password : random_password.grafana_admin[0].result
}

resource "aws_secretsmanager_secret" "grafana_admin" {
  name                    = "options/monitoring/grafana-admin"
  description             = "Grafana admin password."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "grafana_admin" {
  secret_id     = aws_secretsmanager_secret.grafana_admin.id
  secret_string = local.grafana_password

  lifecycle {
    ignore_changes = [secret_string]
  }
}

# ---------------------------------------------------------------------------
# IAM: grant the services EC2 role access to Loki S3 bucket
# ---------------------------------------------------------------------------

data "aws_iam_policy_document" "monitoring_s3" {
  statement {
    actions = [
      "s3:ListBucket",
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
    ]
    resources = [
      aws_s3_bucket.loki.arn,
      "${aws_s3_bucket.loki.arn}/*",
    ]
  }
}

resource "aws_iam_role_policy" "ec2_monitoring_s3" {
  name   = "${var.project}-ec2-monitoring-s3"
  role   = aws_iam_role.ec2.id
  policy = data.aws_iam_policy_document.monitoring_s3.json
}

# ---------------------------------------------------------------------------
# ALB: Grafana target group + path rule  (targets the services EC2)
# ---------------------------------------------------------------------------

resource "aws_lb_target_group" "grafana" {
  name        = "${var.project}-grafana"
  port        = 3000
  protocol    = "HTTP"
  vpc_id      = aws_vpc.main.id
  target_type = "instance"

  deregistration_delay = 30

  health_check {
    path                = "/api/health"
    matcher             = "200"
    interval            = 30
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }
}

resource "aws_lb_target_group_attachment" "grafana" {
  target_group_arn = aws_lb_target_group.grafana.arn
  target_id        = aws_instance.host.id
  port             = 3000
}

resource "aws_lb_listener_rule" "grafana" {
  listener_arn = aws_lb_listener.https.arn
  priority     = 5

  condition {
    path_pattern {
      values = ["/grafana", "/grafana/*"]
    }
  }

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.grafana.arn
  }
}

# ---------------------------------------------------------------------------
# ALB: Gatus status page  (public uptime dashboard)
# ---------------------------------------------------------------------------

resource "aws_lb_target_group" "gatus" {
  name        = "${var.project}-gatus"
  port        = 8080
  protocol    = "HTTP"
  vpc_id      = aws_vpc.main.id
  target_type = "instance"

  deregistration_delay = 30

  health_check {
    path                = "/health"
    matcher             = "200"
    interval            = 30
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }
}

resource "aws_lb_target_group_attachment" "gatus" {
  target_group_arn = aws_lb_target_group.gatus.arn
  target_id        = aws_instance.host.id
  port             = 8080
}

# Host-header rule, not a path rule: gatus can't be served under a path
# prefix (web.context-root was removed upstream and the SPA requests
# assets + /api/* from absolute root paths), so it gets its own
# hostname and serves at / as it expects.
resource "aws_lb_listener_rule" "gatus" {
  listener_arn = aws_lb_listener.https.arn
  priority     = 4

  condition {
    host_header {
      values = ["status.${var.domain_name}"]
    }
  }

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.gatus.arn
  }
}
