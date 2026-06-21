# Per-env app secrets, one per service:
#   options/<env>/indexer     -> {"db_password": "..."}
#   options/<env>/token-info  -> {"db_password": "..."}
#   options/<env>/mm-bot        -> {"sui_key": "...", "quote_key": "..."}
#   options/<env>/oracle-service -> {"pyth_api_key": "..."}
#
# The Pyth API key lives only in oracle-service (the single Pyth gateway) and
# the keeper (created by hand — it keeps a direct Hermes path for the on-chain
# VAA). Every other service reads prices/vol from oracle-service (SO-254).
#
# Terraform creates empty placeholders. The actual values are filled in
# via the AWS console (or `aws secretsmanager put-secret-value`) after
# apply. The deploy script's render-secrets.sh refuses to start a service
# if its secret is missing or malformed, so a forgotten value fails noisy
# rather than silent.
#
# prod/mm-bot is intentionally not created — see services/mm-bot/config/config.prod.toml.

locals {
  envs = ["staging", "prod"]
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
  for_each                = toset(["staging"])
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

# gas-station secret per env — the sponsor (gas payer) key. Placeholder shape,
# fill the real suiprivkey by hand after apply.
resource "aws_secretsmanager_secret" "gas_station" {
  for_each                = toset(local.envs)
  name                    = "options/${each.key}/gas-station"
  description             = "gas-station sponsor signing key (JSON: sui_key)."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "gas_station_placeholder" {
  for_each  = aws_secretsmanager_secret.gas_station
  secret_id = each.value.id
  secret_string = jsonencode({
    sui_key = "REPLACE_ME"
  })
  lifecycle {
    # Operator updates this by hand after apply; don't drift back.
    ignore_changes = [secret_string]
  }
}

# price-charting secret — the Tiger Data TimescaleDB connection URL for the
# OHLC store (SO-156). One Tiger instance per env. Placeholder shape;
# put the real URL by hand after apply:
#   aws secretsmanager put-secret-value --secret-id options/<env>/price-charting \
#     --secret-string '{"database_url":"postgres://..."}'
resource "aws_secretsmanager_secret" "price_charting" {
  for_each                = toset(["staging", "prod"])
  name                    = "options/${each.key}/price-charting"
  description             = "price-charting TimescaleDB URL (JSON: database_url)."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "price_charting_placeholder" {
  for_each  = aws_secretsmanager_secret.price_charting
  secret_id = each.value.id
  secret_string = jsonencode({
    database_url = "REPLACE_ME"
  })
  lifecycle {
    # Operator updates this by hand after apply; don't drift back.
    ignore_changes = [secret_string]
  }
}

# oracle-service secret — the Pyth API key, sent as a Bearer header on the
# single Hermes SSE subscription + Benchmarks requests (SO-254). Placeholder
# shape; put the real key by hand after apply:
#   aws secretsmanager put-secret-value --secret-id options/<env>/oracle-service \
#     --secret-string '{"pyth_api_key":"..."}'
#
# NOTE: these secrets were created out-of-band (via the AWS CLI) during SO-254,
# so on the first `apply` after this lands terraform will report they already
# exist. Import them before applying (the version's ignore_changes then keeps
# the hand-set value):
#   terraform import 'aws_secretsmanager_secret.oracle_service["staging"]' options/staging/oracle-service
#   terraform import 'aws_secretsmanager_secret.oracle_service["prod"]'    options/prod/oracle-service
resource "aws_secretsmanager_secret" "oracle_service" {
  for_each                = toset(local.envs)
  name                    = "options/${each.key}/oracle-service"
  description             = "oracle-service Pyth API key (JSON: pyth_api_key)."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "oracle_service_placeholder" {
  for_each  = aws_secretsmanager_secret.oracle_service
  secret_id = each.value.id
  secret_string = jsonencode({
    pyth_api_key = "REPLACE_ME"
  })
  lifecycle {
    # Operator updates this by hand after apply; don't drift back.
    ignore_changes = [secret_string]
  }
}

# Shared Sui JSON-RPC endpoint (SO-270). One secret per env — every service
# that talks to a Sui fullnode reads this single URL so we point the whole
# fleet at our rate-limit-lifted RPC provider without duplicating the token
# across each service's secret. render-secrets.sh injects it into the [sui]
# block of the keyed service tomls and renders standalone tomls for the
# keyless services. Placeholder shape; put the real URL by hand after apply:
#   aws secretsmanager put-secret-value --secret-id options/<env>/sui-rpc \
#     --secret-string '{"rpc_url":"https://1rpc.io/<token>/sui"}'
# Absent / REPLACE_ME → services fall back to the public Sui endpoint.
resource "aws_secretsmanager_secret" "sui_rpc" {
  for_each                = toset(local.envs)
  name                    = "options/${each.key}/sui-rpc"
  description             = "Shared Sui JSON-RPC endpoint (JSON: rpc_url)."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "sui_rpc_placeholder" {
  for_each  = aws_secretsmanager_secret.sui_rpc
  secret_id = each.value.id
  secret_string = jsonencode({
    rpc_url = "REPLACE_ME"
  })
  lifecycle {
    # Operator updates this by hand after apply; don't drift back.
    ignore_changes = [secret_string]
  }
}
