resource "aws_security_group" "alb" {
  name        = "${var.project}-alb"
  description = "Public ALB ingress."
  vpc_id      = aws_vpc.main.id

  ingress {
    description = "HTTPS from internet"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "HTTP from internet (redirect-to-HTTPS)"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_security_group" "ec2" {
  name = "${var.project}-ec2"
  # AWS treats SG `description` as immutable — editing it here would force
  # SG replacement, which is impossible here (the EC2 host has this SG as
  # its only attached SG, and you can't have zero). If the description ever
  # genuinely needs to change, recreate the host + SG in one go. The actual
  # access control is the ingress rules below; the description is a label.
  description = "Service host. Only the ALB can reach the per-env quoting ports."
  vpc_id      = aws_vpc.main.id

  # Per-env nginx sidecar ports. nginx is the single public entrypoint
  # per env; backend services (quoting, api-service, etc.) talk to nginx
  # over the docker network and don't publish host ports.
  ingress {
    description     = "nginx dev"
    from_port       = 9010
    to_port         = 9010
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }
  ingress {
    description     = "nginx staging"
    from_port       = 9020
    to_port         = 9020
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }
  ingress {
    description     = "nginx prod"
    from_port       = 9030
    to_port         = 9030
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  # Grafana UI (reverse-proxied by the ALB at /grafana/*).
  ingress {
    description     = "Grafana from ALB"
    from_port       = 3000
    to_port         = 3000
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  # Gatus uptime dashboard (reverse-proxied by the ALB at /status/*).
  ingress {
    description     = "Gatus from ALB"
    from_port       = 8080
    to_port         = 8080
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  # Central Loki. The dedicated prod host's Promtail ships container logs
  # to the Loki server on the shared host. Both hosts share this SG, so
  # allow 3100 only between SG members (not the ALB, not the internet).
  ingress {
    description = "Loki from co-SG hosts (prod promtail to shared loki)"
    from_port   = 3100
    to_port     = 3100
    protocol    = "tcp"
    self        = true
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_security_group" "rds" {
  name        = "${var.project}-rds"
  description = "Postgres. Only the EC2 SG can reach it."
  vpc_id      = aws_vpc.main.id

  ingress {
    description     = "postgres from EC2"
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.ec2.id]
  }
}
