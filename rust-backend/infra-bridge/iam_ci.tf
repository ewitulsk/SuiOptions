# The CI deploy role (options-gh-actions-deploy, owned by the MAIN infra root)
# pushes the enclave image here on manual dispatch of bridge-enclave.yml. Its
# ECR push statement is an explicit repo allowlist that does not include this
# root's repo, so we attach a separate named inline policy scoped to just it.
# The main root manages its policies as standalone aws_iam_role_policy
# resources (not exclusive inline_policy blocks), so this addition is safe.
data "aws_iam_policy_document" "gh_actions_bridge_ecr_push" {
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
  role   = "${var.project}-gh-actions-deploy"
  policy = data.aws_iam_policy_document.gh_actions_bridge_ecr_push.json
}
