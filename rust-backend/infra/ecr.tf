locals {
  # NOTE: balance-monitor's repo may already exist in AWS (it was deployed
  # by SO-180 without a terraform entry). If `apply` reports the repo
  # already exists, import it first:
  #   terraform import 'aws_ecr_repository.svc["balance-monitor"]' options/balance-monitor
  # (the lifecycle policy attaches on the next apply).
  # derived-metric-worker was folded into price-charting (SO); its repo is
  # retired. Removing it here destroys the repo on apply — if it still holds
  # images, run `terraform state rm 'aws_ecr_repository.svc["derived-metric-worker"]'`
  # and delete the repo by hand (or set force_delete) to avoid a destroy error.
  service_repos = ["indexer", "quoting-service", "mm-bot", "option-scheduler", "api-service", "token-info", "auth-service", "gas-station", "price-charting", "balance-monitor", "keeper", "oracle-service", "cctp-relay", "twitter-service", "social-bot"]
}

resource "aws_ecr_repository" "svc" {
  for_each             = toset(local.service_repos)
  name                 = "options/${each.key}"
  image_tag_mutability = "MUTABLE"
  image_scanning_configuration {
    scan_on_push = true
  }
}

resource "aws_ecr_lifecycle_policy" "svc" {
  for_each   = aws_ecr_repository.svc
  repository = each.value.name
  policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "Keep last 20 images"
      selection = {
        tagStatus   = "any"
        countType   = "imageCountMoreThan"
        countNumber = 20
      }
      action = { type = "expire" }
    }]
  })
}
