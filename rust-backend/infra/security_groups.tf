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
  name        = "${var.project}-ec2"
  description = "Service host. Only the ALB can reach the per-env quoting ports."
  vpc_id      = aws_vpc.main.id

  # Per-env quoting-service host ports. Source = ALB SG (not 0.0.0.0/0).
  ingress {
    description     = "quoting dev"
    from_port       = 9012
    to_port         = 9012
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }
  ingress {
    description     = "quoting staging"
    from_port       = 9022
    to_port         = 9022
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }
  ingress {
    description     = "quoting prod"
    from_port       = 9032
    to_port         = 9032
    protocol        = "tcp"
    security_groups = [aws_security_group.alb.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_security_group" "runner" {
  name        = "${var.project}-runner"
  description = "Self-hosted GH Actions runner. Egress only (calls out to GH + ECR + Secrets Manager); no inbound."
  vpc_id      = aws_vpc.main.id

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
