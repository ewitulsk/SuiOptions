output "instance_id" {
  description = "Enclave host EC2 instance id."
  value       = aws_instance.enclave.id
}

output "public_ip" {
  value = aws_instance.enclave.public_ip
}

output "private_ip" {
  value = aws_instance.enclave.private_ip
}

output "ecr_repo_url" {
  description = "Set the CI var BRIDGE_ENCLAVE_ECR_REPO to the repo name; push here."
  value       = aws_ecr_repository.enclave.repository_url
}

output "security_group_id" {
  value = aws_security_group.enclave.id
}

output "ssm_session_hint" {
  description = "Reach the host (no SSH — SSM only)."
  value       = "aws ssm start-session --target ${aws_instance.enclave.id} --region ${var.aws_region}"
}
