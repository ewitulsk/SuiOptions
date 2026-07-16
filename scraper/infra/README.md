# scraper/infra

Self-contained Terraform module: single EC2 host running docker compose
(backend, worker, postgres, caddy), ECR, Secrets Manager, GitHub-Actions OIDC
deploy role, SSM-based deploys. Distilled from `rust-backend/infra`.

Not yet implemented — see [../PLAN.md](../PLAN.md) §4–5 and build phase 3.
