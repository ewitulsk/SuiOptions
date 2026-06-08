# ---- EC2 instance role -----------------------------------------------------
#
# The EC2 box needs to:
#  - pull images from ECR
#  - read JSON secrets from Secrets Manager (options/<env>/*)
#  - be reachable by SSM Session Manager + accept ssm send-command
#
# Nothing more. No outbound creds beyond these are stored on the host.

data "aws_iam_policy_document" "ec2_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "ec2" {
  name               = "${var.project}-ec2"
  assume_role_policy = data.aws_iam_policy_document.ec2_assume.json
}

resource "aws_iam_role_policy_attachment" "ec2_ssm" {
  role       = aws_iam_role.ec2.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "ec2_inline" {
  # ECR pull.
  statement {
    actions = [
      "ecr:GetAuthorizationToken",
    ]
    resources = ["*"]
  }
  statement {
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:GetDownloadUrlForLayer",
      "ecr:BatchGetImage",
    ]
    resources = [for r in aws_ecr_repository.svc : r.arn]
  }

  # Secrets Manager: only options/<env>/* JSON entries.
  statement {
    actions = ["secretsmanager:GetSecretValue"]
    resources = [
      "arn:aws:secretsmanager:${var.aws_region}:${data.aws_caller_identity.current.account_id}:secret:options/*",
    ]
  }
}

resource "aws_iam_role_policy" "ec2_inline" {
  name   = "${var.project}-ec2-inline"
  role   = aws_iam_role.ec2.id
  policy = data.aws_iam_policy_document.ec2_inline.json
}

resource "aws_iam_instance_profile" "ec2" {
  name = "${var.project}-ec2"
  role = aws_iam_role.ec2.name
}

data "aws_caller_identity" "current" {}

# ---- GitHub Actions OIDC role ---------------------------------------------

resource "aws_iam_openid_connect_provider" "github" {
  url             = "https://token.actions.githubusercontent.com"
  client_id_list  = ["sts.amazonaws.com"]
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]
}

data "aws_iam_policy_document" "gh_actions_assume" {
  statement {
    actions = ["sts:AssumeRoleWithWebIdentity"]
    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.github.arn]
    }
    condition {
      test     = "StringEquals"
      variable = "token.actions.githubusercontent.com:aud"
      values   = ["sts.amazonaws.com"]
    }
    condition {
      test     = "StringLike"
      variable = "token.actions.githubusercontent.com:sub"
      values   = [for b in var.github_deploy_branches : "repo:${var.github_repo}:ref:refs/heads/${b}"]
    }
  }
}

resource "aws_iam_role" "gh_actions_deploy" {
  name               = "${var.project}-gh-actions-deploy"
  assume_role_policy = data.aws_iam_policy_document.gh_actions_assume.json
}

data "aws_iam_policy_document" "gh_actions_inline" {
  # ECR push to the three repos.
  statement {
    actions = [
      "ecr:GetAuthorizationToken",
    ]
    resources = ["*"]
  }
  statement {
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:CompleteLayerUpload",
      "ecr:InitiateLayerUpload",
      "ecr:PutImage",
      "ecr:UploadLayerPart",
      "ecr:BatchGetImage",
      "ecr:GetDownloadUrlForLayer",
      # DescribeImages lets the deploy-latest workflow look up the most
      # recently-pushed tag without rebuilding.
      "ecr:DescribeImages",
    ]
    resources = [for r in aws_ecr_repository.svc : r.arn]
  }

  # SSM send-command to the EC2 + read invocation results.
  # Match instances by Name tag rather than ARN — EC2 replacements (AMI
  # bumps, user_data changes that opt into replace_on_change, etc.) change
  # the instance ID and otherwise leave the policy stale. The tag is set
  # in ec2.tf on aws_instance.host.
  statement {
    actions = ["ssm:SendCommand"]
    resources = [
      "arn:aws:ec2:${var.aws_region}:${data.aws_caller_identity.current.account_id}:instance/*",
    ]
    condition {
      test     = "StringEquals"
      variable = "ssm:resourceTag/Name"
      values   = ["${var.project}-host", "${var.project}-prod-host"]
    }
  }
  statement {
    actions = ["ssm:SendCommand"]
    resources = [
      "arn:aws:ssm:${var.aws_region}::document/AWS-RunShellScript",
    ]
  }
  statement {
    actions = [
      "ssm:GetCommandInvocation",
      "ssm:ListCommandInvocations",
    ]
    resources = ["*"]
  }

  # Resolve the host's instance ID by Name tag at run time (see
  # .github/actions/resolve-ec2). DescribeInstances is an API-level read
  # with no resource-level scoping, so it must be granted on "*".
  statement {
    actions   = ["ec2:DescribeInstances"]
    resources = ["*"]
  }

  # SSM output bucket (created below).
  statement {
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:ListBucket",
    ]
    resources = [
      aws_s3_bucket.ssm_output.arn,
      "${aws_s3_bucket.ssm_output.arn}/*",
    ]
  }

  # Secrets Manager: read options/<env>/* JSON entries. The redeploy-contract
  # workflow assembles secrets.toml on the runner (scheduler deployer key) and
  # reads the DB password, so it needs the same read access the EC2 role has.
  statement {
    actions = ["secretsmanager:GetSecretValue"]
    resources = [
      "arn:aws:secretsmanager:${var.aws_region}:${data.aws_caller_identity.current.account_id}:secret:options/*",
    ]
  }
}

resource "aws_iam_role_policy" "gh_actions_inline" {
  name   = "${var.project}-gh-actions-inline"
  role   = aws_iam_role.gh_actions_deploy.id
  policy = data.aws_iam_policy_document.gh_actions_inline.json
}

# Small S3 bucket for SSM command output (workflow tails it for logs).
resource "aws_s3_bucket" "ssm_output" {
  bucket_prefix = "${var.project}-ssm-output-"
}

resource "aws_s3_bucket_lifecycle_configuration" "ssm_output" {
  bucket = aws_s3_bucket.ssm_output.id

  rule {
    id     = "expire-old-output"
    status = "Enabled"

    filter {}

    expiration {
      days = 14
    }
  }
}

# Let the EC2 role write SSM command output to the bucket, and read
# deploy bundles (deploy.sh / render-secrets.sh / compose files) that
# the GitHub Actions workflow uploads under `deploy-bundles/<sha>.tgz`.
# The bundle sync runs at the top of every selective deploy so the box
# always uses the scripts at the SHA being deployed.
resource "aws_iam_role_policy" "ec2_ssm_output" {
  name = "${var.project}-ec2-ssm-output"
  role = aws_iam_role.ec2.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = ["s3:PutObject", "s3:GetEncryptionConfiguration"]
        Resource = [
          aws_s3_bucket.ssm_output.arn,
          "${aws_s3_bucket.ssm_output.arn}/*",
        ]
      },
      {
        Effect = "Allow"
        Action = ["s3:GetObject"]
        Resource = [
          "${aws_s3_bucket.ssm_output.arn}/deploy-bundles/*",
        ]
      },
    ]
  })
}
