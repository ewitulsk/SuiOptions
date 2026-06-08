locals {
  service_repos = ["indexer", "quoting-service", "mm-bot", "option-scheduler", "api-service", "token-info", "auth-service"]
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
