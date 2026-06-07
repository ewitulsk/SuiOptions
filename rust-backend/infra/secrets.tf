# Per-env app secrets, one per service:
#   options/<env>/indexer     -> {"db_password": "..."}
#   options/<env>/token-info  -> {"db_password": "..."}
#   options/<env>/mm-bot      -> {"sui_key": "...", "quote_key": "..."}
#
# Terraform creates empty placeholders. The actual values are filled in
# via the AWS console (or `aws secretsmanager put-secret-value`) after
# apply. The deploy script's render-secrets.sh refuses to start a service
# if its secret is missing or malformed, so a forgotten value fails noisy
# rather than silent.
#
# prod/mm-bot is intentionally not created — see services/mm-bot/config/config.prod.toml.

locals {
  envs = ["dev", "staging", "prod"]
}

# Indexer secret per env.
resource "aws_secretsmanager_secret" "indexer" {
  for_each                = toset(local.envs)
  name                    = "options/${each.key}/indexer"
  description             = "Indexer DB password (JSON: db_password)."
  recovery_window_in_days = 7
}

resource "random_password" "indexer_db" {
  for_each = toset(local.envs)
  length   = 32
  special  = false
}

resource "aws_secretsmanager_secret_version" "indexer" {
  for_each  = aws_secretsmanager_secret.indexer
  secret_id = each.value.id
  secret_string = jsonencode({
    db_password = random_password.indexer_db[each.key].result
  })
}

# token-info secret per env.
resource "aws_secretsmanager_secret" "token_info" {
  for_each                = toset(local.envs)
  name                    = "options/${each.key}/token-info"
  description             = "token-info DB password (JSON: db_password)."
  recovery_window_in_days = 7
}

resource "random_password" "token_info_db" {
  for_each = toset(local.envs)
  length   = 32
  special  = false
}

resource "aws_secretsmanager_secret_version" "token_info" {
  for_each  = aws_secretsmanager_secret.token_info
  secret_id = each.value.id
  secret_string = jsonencode({
    db_password = random_password.token_info_db[each.key].result
  })
}

# auth-service secret per env — JWT signing key, auto-generated.
resource "aws_secretsmanager_secret" "auth_service" {
  for_each                = toset(local.envs)
  name                    = "options/${each.key}/auth-service"
  description             = "auth-service JWT signing secret (JSON: jwt_secret)."
  recovery_window_in_days = 7
}

resource "random_password" "auth_jwt" {
  for_each = toset(local.envs)
  length   = 48
  special  = false
}

resource "aws_secretsmanager_secret_version" "auth_service" {
  for_each  = aws_secretsmanager_secret.auth_service
  secret_id = each.value.id
  secret_string = jsonencode({
    jwt_secret = random_password.auth_jwt[each.key].result
  })
}

# mm-bot secret per env — placeholder shape, fill values by hand.
resource "aws_secretsmanager_secret" "mm_bot" {
  for_each                = toset(["dev", "staging"])
  name                    = "options/${each.key}/mm-bot"
  description             = "mm-bot signing keys (JSON: sui_key, quote_key)."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "mm_bot_placeholder" {
  for_each  = aws_secretsmanager_secret.mm_bot
  secret_id = each.value.id
  secret_string = jsonencode({
    sui_key   = "REPLACE_ME"
    quote_key = "REPLACE_ME"
  })
  lifecycle {
    # Operator updates this by hand after apply; don't drift back.
    ignore_changes = [secret_string]
  }
}
