# GitHub PAT used by the self-hosted runner to mint registration tokens.
# Terraform creates the secret as a REPLACE_ME placeholder; populate by
# hand after apply (see infra/README.md). The runner's register script
# refuses to register if the PAT still reads REPLACE_ME, so a forgotten
# fill fails noisy on first boot rather than silently.
#
# Required PAT scopes:
#  - Classic PAT: `repo`
#  - Fine-grained PAT: Repository → Administration: Read and write
#    (this is the scope that gates self-hosted runner management)

resource "aws_secretsmanager_secret" "github_runner_pat" {
  name                    = "options/ci/github-runner-pat"
  description             = "GitHub PAT for self-hosted runner registration. JSON: {\"pat\": \"...\"}"
  recovery_window_in_days = 7
}

resource "aws_secretsmanager_secret_version" "github_runner_pat_placeholder" {
  secret_id = aws_secretsmanager_secret.github_runner_pat.id
  secret_string = jsonencode({
    pat = "REPLACE_ME"
  })
  lifecycle {
    ignore_changes = [secret_string]
  }
}
