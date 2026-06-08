resource "aws_lb" "alb" {
  name               = "${var.project}-alb"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [aws_security_group.alb.id]
  subnets            = aws_subnet.public[*].id

  idle_timeout = 300 # WS connections need >60s; bump well above that.

  # Access logs land in S3; the bucket policy below grants the regional
  # ELB service account permission to write. Without this, traffic that
  # induces failures (e.g. the SO-65 RFQ burst) leaves no trace of which
  # client IP / path produced it.
  access_logs {
    bucket  = aws_s3_bucket.alb_logs.bucket
    prefix  = "alb"
    enabled = true
  }

  tags = {
    Name = "${var.project}-alb"
  }

  # Force ordering: the bucket policy must exist before the LB tries to
  # write its first probe log on apply, otherwise terraform errors with
  # "Access Denied for bucket: ...".
  depends_on = [aws_s3_bucket_policy.alb_logs]
}

# ---- Access logs bucket -------------------------------------------------

resource "aws_s3_bucket" "alb_logs" {
  bucket_prefix = "${var.project}-alb-logs-"
}

resource "aws_s3_bucket_lifecycle_configuration" "alb_logs" {
  bucket = aws_s3_bucket.alb_logs.id

  rule {
    id     = "expire-old-logs"
    status = "Enabled"

    filter {}

    expiration {
      days = 30
    }
  }
}

resource "aws_s3_bucket_public_access_block" "alb_logs" {
  bucket                  = aws_s3_bucket.alb_logs.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# The ELB service in each region writes from a fixed AWS account. We use
# `aws_elb_service_account` to pick the right one rather than hardcoding.
data "aws_elb_service_account" "main" {}

data "aws_iam_policy_document" "alb_logs" {
  statement {
    sid     = "AllowELBLogWrite"
    actions = ["s3:PutObject"]
    resources = [
      "${aws_s3_bucket.alb_logs.arn}/alb/AWSLogs/${data.aws_caller_identity.current.account_id}/*",
    ]
    principals {
      type        = "AWS"
      identifiers = [data.aws_elb_service_account.main.arn]
    }
  }
}

resource "aws_s3_bucket_policy" "alb_logs" {
  bucket = aws_s3_bucket.alb_logs.id
  policy = data.aws_iam_policy_document.alb_logs.json
}

# ---- One target group + rule per env ------------------------------------
#
# Public URL shape: https://<domain>/<env>/<service>/<endpoint>
#   <env>     in {dev, staging, prod}
#   <service> in {quoting, api}
#
# Each env has a single nginx sidecar container as the public entrypoint;
# the ALB forwards /<env>/* to that env's nginx host port and nginx does
# the per-service dispatch + prefix stripping (deployment/nginx/*.conf).

locals {
  alb_envs = {
    dev     = { nginx_port = 9010, priority = 10 }
    staging = { nginx_port = 9020, priority = 20 }
    prod    = { nginx_port = 9030, priority = 30 }
  }
}

resource "aws_lb_target_group" "ingress" {
  for_each    = local.alb_envs
  name        = "${var.project}-ingress-${each.key}"
  port        = each.value.nginx_port
  protocol    = "HTTP"
  vpc_id      = aws_vpc.main.id
  target_type = "instance"

  deregistration_delay = 30

  # nginx serves /<env>/health directly with a fixed 200 (see
  # deployment/nginx/nginx.<env>.conf). Doesn't probe upstream services —
  # that's deploy.sh's job during the deploy window.
  health_check {
    path                = "/${each.key}/health"
    matcher             = "200"
    interval            = 15
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }

  # `name` change forces TG replacement. Without create_before_destroy
  # terraform tries to destroy the old TG first, which AWS rejects because
  # the listener rule still references it (the rule's target_group_arn
  # can't be flipped to a new ARN until that ARN exists). cbd lets the
  # new TG come up first, the rule update in-place, then the old TG drops
  # out cleanly. Old and new names differ so both can coexist briefly.
  lifecycle {
    create_before_destroy = true
  }
}

resource "aws_lb_target_group_attachment" "ingress" {
  for_each         = local.alb_envs
  target_group_arn = aws_lb_target_group.ingress[each.key].arn
  # prod lives on its own instance; dev/staging stay on the shared host.
  target_id = each.key == "prod" ? aws_instance.prod_host.id : aws_instance.host.id
  port      = each.value.nginx_port

  # Mirrors the TG's create_before_destroy — the attachment's
  # target_group_arn references a TG that's being replaced, so the
  # attachment must be replaced in the same ordering or terraform may
  # try to destroy this attachment before the new TG (and thus the new
  # attachment) exists.
  lifecycle {
    create_before_destroy = true
  }
}

# Migrate the existing per-env quoting TG/attachment/rule to the new
# ingress resource addresses. The AWS-side TG name (options-quoting-<env>
# vs options-ingress-<env>) is forced to change, so apply will replace
# the TG — brief 503s during deregistration. Apply during a quiet window.
moved {
  from = aws_lb_target_group.quoting["dev"]
  to   = aws_lb_target_group.ingress["dev"]
}
moved {
  from = aws_lb_target_group.quoting["staging"]
  to   = aws_lb_target_group.ingress["staging"]
}
moved {
  from = aws_lb_target_group.quoting["prod"]
  to   = aws_lb_target_group.ingress["prod"]
}
moved {
  from = aws_lb_target_group_attachment.quoting["dev"]
  to   = aws_lb_target_group_attachment.ingress["dev"]
}
moved {
  from = aws_lb_target_group_attachment.quoting["staging"]
  to   = aws_lb_target_group_attachment.ingress["staging"]
}
moved {
  from = aws_lb_target_group_attachment.quoting["prod"]
  to   = aws_lb_target_group_attachment.ingress["prod"]
}

# ---- Listeners + path rules ---------------------------------------------

resource "aws_lb_listener" "http_redirect" {
  load_balancer_arn = aws_lb.alb.arn
  port              = 80
  protocol          = "HTTP"
  default_action {
    type = "redirect"
    redirect {
      port        = "443"
      protocol    = "HTTPS"
      status_code = "HTTP_301"
    }
  }
}

resource "aws_lb_listener" "https" {
  load_balancer_arn = aws_lb.alb.arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = aws_acm_certificate_validation.alb.certificate_arn

  default_action {
    type = "fixed-response"
    fixed_response {
      content_type = "text/plain"
      message_body = "Not found"
      status_code  = "404"
    }
  }
}

resource "aws_lb_listener_rule" "ingress" {
  for_each     = local.alb_envs
  listener_arn = aws_lb_listener.https.arn
  priority     = each.value.priority

  condition {
    path_pattern {
      values = ["/${each.key}/*"]
    }
  }

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.ingress[each.key].arn
  }
}

moved {
  from = aws_lb_listener_rule.path["dev"]
  to   = aws_lb_listener_rule.ingress["dev"]
}
moved {
  from = aws_lb_listener_rule.path["staging"]
  to   = aws_lb_listener_rule.ingress["staging"]
}
moved {
  from = aws_lb_listener_rule.path["prod"]
  to   = aws_lb_listener_rule.ingress["prod"]
}
