# Dedicated OIDC deploy role for bridge-enclave.yml (the workflow header's
# "its own ECR repo + IAM role from the isolated infra-bridge root"). The main
# root's options-gh-actions-deploy only trusts refs/heads/{staging,main}, and
# its trust policy is main-root-managed — so the bridge deploy gets its own
# role instead of an out-of-band edit there. Set the repo var
# BRIDGE_DEPLOY_ROLE_ARN to this role's ARN.
data "aws_iam_openid_connect_provider" "github" {
  url = "https://token.actions.githubusercontent.com"
}

data "aws_iam_policy_document" "bridge_gh_assume" {
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [data.aws_iam_openid_connect_provider.github.arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values = [
        "repo:ewitulsk/SuiOptions:ref:refs/heads/ewitulsk/sui-bridge",
        "repo:ewitulsk/SuiOptions:ref:refs/heads/staging",
        "repo:ewitulsk/SuiOptions:ref:refs/heads/main",
      ]
    }
  }
}

resource "aws_iam_role" "bridge_gh_deploy" {
  name               = "${var.project}-bridge-gh-deploy"
  assume_role_policy = data.aws_iam_policy_document.bridge_gh_assume.json
}

data "aws_iam_policy_document" "gh_actions_bridge_ecr_push" {
  statement {
    sid       = "EcrAuth"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }
  statement {
    sid = "BridgeEnclaveEcrPush"
    actions = [
      "ecr:UploadLayerPart",
      "ecr:PutImage",
      "ecr:InitiateLayerUpload",
      "ecr:GetDownloadUrlForLayer",
      "ecr:DescribeImages",
      "ecr:CompleteLayerUpload",
      "ecr:BatchGetImage",
      "ecr:BatchCheckLayerAvailability",
    ]
    resources = [aws_ecr_repository.enclave.arn]
  }
}

resource "aws_iam_role_policy" "gh_actions_bridge_ecr_push" {
  name   = "${var.project}-bridge-enclave-ecr-push"
  role   = aws_iam_role.bridge_gh_deploy.id
  policy = data.aws_iam_policy_document.gh_actions_bridge_ecr_push.json
}

output "bridge_deploy_role_arn" {
  description = "Set the repo var BRIDGE_DEPLOY_ROLE_ARN to this."
  value       = aws_iam_role.bridge_gh_deploy.arn
}
