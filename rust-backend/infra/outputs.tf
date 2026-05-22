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
  description = "Where the quoting service is reachable per env."
  value = {
    dev     = "https://${var.domain_name}/dev/"
    staging = "https://${var.domain_name}/staging/"
    prod    = "https://${var.domain_name}/prod/"
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
    [for env in ["dev", "staging"] : "options/${env}/mm-bot (REPLACE_ME — fill sui_key + quote_key)"],
    ["options/ci/github-runner-pat (REPLACE_ME — fill pat; see infra/README.md)"]
  )
}

output "runner_spot_request_id" {
  description = "Spot request ID for the GH Actions runner."
  value       = aws_spot_instance_request.runner.id
}

output "runner_instance_id" {
  description = "EC2 instance ID currently fulfilling the runner spot request."
  value       = aws_spot_instance_request.runner.spot_instance_id
}
