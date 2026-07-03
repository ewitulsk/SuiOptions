# Standalone ECR repo for the enclave app image. Kept in THIS root (not the main
# infra's `aws_ecr_repository.svc` for_each map) so we don't touch the drift
# landmine there. The CI (bridge-enclave.yml) pushes here on manual deploy.
resource "aws_ecr_repository" "enclave" {
  name                 = var.ecr_repo_name
  image_tag_mutability = "IMMUTABLE" # pin-by-digest discipline for reproducible PCRs
  force_delete         = false

  image_scanning_configuration {
    scan_on_push = true
  }

  tags = { Name = var.ecr_repo_name }
}
