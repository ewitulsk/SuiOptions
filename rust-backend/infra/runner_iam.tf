# ---- Runner instance role --------------------------------------------------
#
# The runner box needs to:
#  - read the GitHub PAT from Secrets Manager (options/ci/*)
#  - be reachable via SSM Session Manager (debug shell, no SSH required)
#
# It does NOT push to ECR via the instance role — the workflow uses the
# existing gh-actions-deploy OIDC role for that, same as on GH-hosted.
# Keeps the runner's blast radius to: register itself, run jobs.

data "aws_iam_policy_document" "runner_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "runner" {
  name               = "${var.project}-runner-ec2"
  assume_role_policy = data.aws_iam_policy_document.runner_assume.json
}

resource "aws_iam_role_policy_attachment" "runner_ssm" {
  role       = aws_iam_role.runner.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "runner_inline" {
  statement {
    actions   = ["secretsmanager:GetSecretValue"]
    resources = [aws_secretsmanager_secret.github_runner_pat.arn]
  }
}

resource "aws_iam_role_policy" "runner_inline" {
  name   = "${var.project}-runner-inline"
  role   = aws_iam_role.runner.id
  policy = data.aws_iam_policy_document.runner_inline.json
}

resource "aws_iam_instance_profile" "runner" {
  name = "${var.project}-runner-ec2"
  role = aws_iam_role.runner.name
}
