# infra/

Terraform that stands up the whole stack: VPC, EC2, RDS Postgres, ECR,
Secrets Manager, ALB + ACM + Route 53, and the GitHub Actions OIDC role.

See `../deployment.md` for what each piece does and why. This README is
just the apply runbook.

## Prereqs

- Terraform ≥ 1.6 (`brew install terraform` or `tfenv install 1.6.6`).
- AWS CLI authenticated as an account-admin-ish identity (Terraform
  creates IAM roles, so a read-only profile will fail).
- A DNS zone for the domain you'll serve from. The cleanest path is
  Route 53; if your zone lives elsewhere, see the manual-DNS note below.
- The repo's `staging` and `main` branches must already exist on GitHub
  before the OIDC trust policy is exercised (the role is created either
  way, but pushes can't assume it until the branches do).

## First apply

```bash
cd infra
cp terraform.tfvars.example terraform.tfvars
# edit terraform.tfvars: set domain_name, github_repo, route53_zone_id
terraform init
terraform plan
terraform apply
```

`terraform apply` typically pauses for ~5 minutes during ACM cert
validation while the DNS records propagate. That's normal.

## After apply

1. **Wire the GitHub repository variables** (Settings → Secrets and
   variables → Actions → Variables). Use the Terraform outputs:
   ```
   AWS_REGION         = us-east-1
   ECR_REGISTRY       = <ecr_registry output>
   EC2_INSTANCE_ID    = <ec2_instance_id output>
   DEPLOY_ROLE_ARN    = <gh_actions_deploy_role_arn output>
   RDS_HOST           = <rds_endpoint output>
   SSM_OUTPUT_BUCKET  = <ssm_output_bucket output>
   ```
   No GH secrets needed — OIDC handles AWS auth.

2. **Create per-env Postgres DBs and users.** Connect to the RDS instance
   from the EC2 box (SSM Session Manager → instance → start session):
   ```bash
   sudo apt-get install -y postgresql-client
   MASTER=$(aws secretsmanager get-secret-value \
     --secret-id options/_master/db --query SecretString --output text)
   HOST=$(echo "$MASTER" | jq -r .host)
   PASS=$(echo "$MASTER" | jq -r .password)
   PGPASSWORD="$PASS" psql -h "$HOST" -U postgres -d postgres <<SQL
   CREATE DATABASE indexer_dev;
   CREATE DATABASE indexer_staging;
   CREATE DATABASE indexer_prod;
   CREATE USER indexer_dev     WITH PASSWORD '$(aws secretsmanager get-secret-value --secret-id options/dev/indexer     --query SecretString --output text | jq -r .db_password)';
   CREATE USER indexer_staging WITH PASSWORD '$(aws secretsmanager get-secret-value --secret-id options/staging/indexer --query SecretString --output text | jq -r .db_password)';
   CREATE USER indexer_prod    WITH PASSWORD '$(aws secretsmanager get-secret-value --secret-id options/prod/indexer    --query SecretString --output text | jq -r .db_password)';
   GRANT ALL PRIVILEGES ON DATABASE indexer_dev     TO indexer_dev;
   GRANT ALL PRIVILEGES ON DATABASE indexer_staging TO indexer_staging;
   GRANT ALL PRIVILEGES ON DATABASE indexer_prod    TO indexer_prod;
   SQL
   ```

3. **Fill in the mm-bot secrets.** Terraform created placeholders for
   `options/dev/mm-bot` and `options/staging/mm-bot`. Generate keys
   locally and upload:
   ```bash
   SUI_KEY=$(sui keytool generate ed25519 --json | jq -r '.[0].suiPrivateKey')
   QUOTE_KEY=$(openssl rand -hex 32)
   aws secretsmanager put-secret-value \
     --secret-id options/dev/mm-bot \
     --secret-string "{\"sui_key\":\"$SUI_KEY\",\"quote_key\":\"$QUOTE_KEY\"}"
   # repeat for staging
   ```

4. **Trigger the first deploy** by pushing to `staging` (deploys dev +
   staging) or by running the GH Actions workflow manually with
   `workflow_dispatch`. The first build is ~10 min (ARM via QEMU emulation
   on x86 runners).

## Manual DNS note

If `route53_zone_id` is empty, Terraform creates the ACM cert but
doesn't auto-add the validation CNAME. After `terraform apply` pauses on
`aws_acm_certificate_validation.alb`, run:

```bash
terraform show -json aws_acm_certificate.alb \
  | jq '.values.domain_validation_options[]'
```

Copy each `resource_record_name`/`resource_record_value` pair into your
DNS provider as a CNAME. Once they propagate (usually <5 min), the
apply resumes. Then add an A or CNAME for `<domain_name>` pointing at
the `alb_dns_name` output.

## Cleanup

```bash
terraform destroy
```

Two things Terraform leaves behind on purpose:
- **ECR images**: the lifecycle policy keeps the last 20 tags. To wipe
  them, manually delete the repos before destroying, or set
  `force_delete = true` on each `aws_ecr_repository`.
- **RDS final snapshot**: `skip_final_snapshot = true` for cheapness;
  destroy drops the DB cleanly with no snapshot left behind. Flip this
  for prod when you care.
