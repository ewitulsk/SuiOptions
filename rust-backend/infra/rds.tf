resource "aws_db_subnet_group" "main" {
  name       = "${var.project}-db"
  subnet_ids = aws_subnet.private[*].id
  tags = {
    Name = "${var.project}-db-subnet-group"
  }
}

resource "random_password" "db_master" {
  length      = 32
  special     = false
  min_lower   = 4
  min_upper   = 4
  min_numeric = 4
}

# Cheap-tier RDS: standard Postgres on db.t4g.micro, single-AZ.
resource "aws_db_instance" "main" {
  identifier              = "${var.project}-db"
  engine                  = "postgres"
  engine_version          = "16"
  instance_class          = var.rds_instance_class
  allocated_storage       = var.rds_allocated_storage_gb
  storage_type            = "gp3"
  storage_encrypted       = true
  multi_az                = false
  publicly_accessible     = false
  vpc_security_group_ids  = [aws_security_group.rds.id]
  db_subnet_group_name    = aws_db_subnet_group.main.name
  username                = "postgres"
  password                = random_password.db_master.result
  backup_retention_period = var.rds_backup_retention_days
  skip_final_snapshot     = true
  deletion_protection     = false
  apply_immediately       = true

  parameter_group_name = aws_db_parameter_group.main.name
  tags = {
    Name = "${var.project}-db"
  }
}

resource "aws_db_parameter_group" "main" {
  name   = "${var.project}-pg16"
  family = "postgres16"
}

# Hold the master credentials in Secrets Manager so the operator can
# create per-env DBs/users on first run without copy-pasting from
# `terraform output`.
resource "aws_secretsmanager_secret" "db_master" {
  name                    = "options/_master/db"
  description             = "RDS master credentials. Used once for bootstrap; per-env credentials live in options/<env>/indexer."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "db_master" {
  secret_id = aws_secretsmanager_secret.db_master.id
  secret_string = jsonencode({
    username = aws_db_instance.main.username
    password = random_password.db_master.result
    host     = aws_db_instance.main.address
    port     = aws_db_instance.main.port
  })
}
