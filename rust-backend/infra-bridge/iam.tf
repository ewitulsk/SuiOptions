data "aws_iam_policy_document" "ec2_assume" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "enclave" {
  name               = "${var.project}-bridge-enclave"
  assume_role_policy = data.aws_iam_policy_document.ec2_assume.json
}

# SSM manages the host (no SSH). AL2023 ships the SSM agent.
resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.enclave.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# ECR pull of the enclave image (scoped to our repo; the auth-token call is
# account-wide and cannot be resource-scoped).
data "aws_iam_policy_document" "enclave_inline" {
  statement {
    sid       = "EcrAuth"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }
  statement {
    sid = "EcrPull"
    actions = [
      "ecr:BatchGetImage",
      "ecr:GetDownloadUrlForLayer",
      "ecr:BatchCheckLayerAvailability",
    ]
    resources = [aws_ecr_repository.enclave.arn]
  }
}

resource "aws_iam_role_policy" "enclave_inline" {
  name   = "${var.project}-bridge-enclave"
  role   = aws_iam_role.enclave.id
  policy = data.aws_iam_policy_document.enclave_inline.json
}

resource "aws_iam_instance_profile" "enclave" {
  name = "${var.project}-bridge-enclave"
  role = aws_iam_role.enclave.name
}
