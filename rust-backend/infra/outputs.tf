output "ec2_instance_id" {
  description = "Set this as GH Actions repository variable EC2_INSTANCE_ID."
  value       = aws_instance.host.id
}

output "ec2_public_ip" {
  description = "Public IP of the host. Inbound is locked to the ALB SG."
  value       = aws_instance.host.public_ip
}

output "alb_dns_name" {
  description = "ALB DNS. Useful if route53_zone_id is empty."
  value       = aws_lb.alb.dns_name
}

output "alb_zone_id" {
  description = "Hosted zone of the ALB; alias targets need this."
  value       = aws_lb.alb.zone_id
}

output "domain_url" {
  description = "Per-env public URLs. quoting is WS, api is HTTP."
  value = {
    staging = {
      quoting = "wss://${var.domain_name}/staging/quoting"
      api     = "https://${var.domain_name}/staging/api"
    }
    prod = {
      quoting = "wss://${var.domain_name}/prod/quoting"
      api     = "https://${var.domain_name}/prod/api"
    }
  }
}

output "rds_endpoint" {
  description = "Set this as GH Actions repository variable RDS_HOST."
  value       = aws_db_instance.main.address
}

output "rds_port" {
  value = aws_db_instance.main.port
}

output "ecr_registry" {
  description = "Set this as GH Actions repository variable ECR_REGISTRY."
  value       = "${data.aws_caller_identity.current.account_id}.dkr.ecr.${var.aws_region}.amazonaws.com"
}

output "gh_actions_deploy_role_arn" {
  description = "Set this as GH Actions repository variable DEPLOY_ROLE_ARN."
  value       = aws_iam_role.gh_actions_deploy.arn
}

output "ssm_output_bucket" {
  description = "Set this as GH Actions repository variable SSM_OUTPUT_BUCKET."
  value       = aws_s3_bucket.ssm_output.bucket
}

output "secrets_to_fill" {
  description = "Secrets Manager entries the operator must populate by hand after apply."
  value = concat(
    [for env in local.envs : "options/${env}/indexer (auto-populated; rotate via random_password.indexer_db)"],
    [for env in local.envs : "options/${env}/token-info (auto-populated; rotate via random_password.token_info_db)"],
    [for env in ["staging"] : "options/${env}/mm-bot (REPLACE_ME — fill sui_key + quote_key)"],
    ["options/_master/tailscale-auth-key (REPLACE_ME — fill auth_key from tailscale admin console)"],
  )
}

# ---- Monitoring outputs ------------------------------------------------------

output "grafana_url" {
  description = "Grafana URL via the ALB."
  value       = "https://${var.domain_name}/grafana/"
}

output "loki_bucket" {
  description = "S3 bucket backing Loki chunk + index storage."
  value       = aws_s3_bucket.loki.bucket
}

output "grafana_admin_secret" {
  description = "Secrets Manager entry holding the Grafana admin password."
  value       = aws_secretsmanager_secret.grafana_admin.name
}

output "tailscale_setup" {
  description = "One-time Tailscale wiring after apply. See infra/README.md for the full runbook."
  value = {
    auth_key_secret = aws_secretsmanager_secret.tailscale_auth_key.name
    advertised_cidr = var.vpc_cidr
    rds_endpoint    = aws_db_instance.main.address
    admin_console   = "https://login.tailscale.com/admin/machines"
  }
}
