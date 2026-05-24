# Tailscale subnet router. The EC2 host advertises the VPC CIDR onto our
# tailnet so teammates can reach in-VPC resources (RDS in particular)
# from their laptops, without exposing them publicly.
#
# Tailscale-side setup is manual and lives in the admin console:
#   1. Define a tag for the router in the tailnet policy file:
#        "tagOwners": { "tag:options-router": ["autogroup:admin"] }
#   2. Mint a reusable, non-ephemeral, pre-approved auth key tagged
#      tag:options-router  (Settings -> Keys -> Generate auth key).
#   3. Stash the key in this Secrets Manager entry:
#        aws secretsmanager put-secret-value \
#          --secret-id options/_master/tailscale-auth-key \
#          --secret-string '{"auth_key":"tskey-auth-..."}'
#   4. After the EC2 picks up the key and registers, approve the
#      advertised subnet route in Machines -> options-router -> Edit
#      route settings.
#   5. Invite teammates via Users -> Invite (Tailscale emails them a
#      link to join the tailnet).
#
# The secret intentionally starts as a REPLACE_ME placeholder so a
# fresh apply doesn't fail; the systemd unit on the box no-ops noisily
# until a real key is populated.
resource "aws_secretsmanager_secret" "tailscale_auth_key" {
  name                    = "options/_master/tailscale-auth-key"
  description             = "Tailscale auth key for the EC2 subnet router. JSON: {auth_key}."
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "tailscale_auth_key_placeholder" {
  secret_id = aws_secretsmanager_secret.tailscale_auth_key.id
  secret_string = jsonencode({
    auth_key = "REPLACE_ME"
  })
  lifecycle {
    # Operator rotates this by hand; don't drift back to the placeholder.
    ignore_changes = [secret_string]
  }
}
