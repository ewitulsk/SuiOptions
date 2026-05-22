resource "aws_lb" "alb" {
  name               = "${var.project}-alb"
  internal           = false
  load_balancer_type = "application"
  security_groups    = [aws_security_group.alb.id]
  subnets            = aws_subnet.public[*].id

  idle_timeout = 300 # WS connections need >60s; bump well above that.

  tags = {
    Name = "${var.project}-alb"
  }
}

# ---- One target group per env --------------------------------------------

locals {
  alb_envs = {
    dev     = { port = 9012 }
    staging = { port = 9022 }
    prod    = { port = 9032 }
  }
}

resource "aws_lb_target_group" "quoting" {
  for_each    = local.alb_envs
  name        = "${var.project}-quoting-${each.key}"
  port        = each.value.port
  protocol    = "HTTP"
  vpc_id      = aws_vpc.main.id
  target_type = "instance"

  deregistration_delay = 30

  health_check {
    path                = "/health"
    matcher             = "200"
    interval            = 15
    timeout             = 5
    healthy_threshold   = 2
    unhealthy_threshold = 3
  }
}

resource "aws_lb_target_group_attachment" "quoting" {
  for_each         = local.alb_envs
  target_group_arn = aws_lb_target_group.quoting[each.key].arn
  target_id        = aws_instance.host.id
  port             = each.value.port
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

resource "aws_lb_listener_rule" "path" {
  for_each     = local.alb_envs
  listener_arn = aws_lb_listener.https.arn
  priority     = each.key == "dev" ? 10 : (each.key == "staging" ? 20 : 30)

  condition {
    path_pattern {
      values = ["/${each.key}/*"]
    }
  }

  action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.quoting[each.key].arn
  }
}
