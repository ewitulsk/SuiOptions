# ---- Data room (SO-388) -----------------------------------------------------
#
# Market-data lake stack (docs/data-room-spec.md §10): an S3 bucket for the
# bronze/silver/gold Parquet archive, plus a dedicated `data-room-host` EC2
# that runs the collector + batch containers. Deliberately a peer stack, not
# a peer service: own host, own SG, own instance role scoped to the bucket —
# no RDS, no ALB, no secrets, and NOT wired into the gatus/user-data
# machinery on the protocol hosts (editing that bounces them).
#
# The two ECR repos (data-room-collector, data-room-batch) live in ecr.tf
# alongside the other service repos so CI push via the GH Actions role works.

# ---------------------------------------------------------------------------
# S3 bucket for the market-data lake
# ---------------------------------------------------------------------------

resource "aws_s3_bucket" "data_room" {
  bucket_prefix = "${var.project}-data-room-"
  force_destroy = false

  tags = {
    Name = "${var.project}-data-room"
  }
}

resource "aws_s3_bucket_versioning" "data_room" {
  bucket = aws_s3_bucket.data_room.id
  versioning_configuration {
    status = "Enabled"
  }
}

# bronze/ is the append-only verbatim archive: reads taper off fast, so tier
# it down (STANDARD_IA at 30d, GLACIER_IR at 180d). bronze objects are never
# rewritten, so any noncurrent version there is an accident — expire it
# after 1 day. silver/gold ARE rewritten (regenerated from bronze), so keep
# their noncurrent versions 30 days as an undo window. Where the bronze rule
# and the catch-all overlap, S3 applies the earliest expiration (1 day wins
# under bronze/); the catch-all carries no transitions, so no conflict.
resource "aws_s3_bucket_lifecycle_configuration" "data_room" {
  bucket = aws_s3_bucket.data_room.id

  rule {
    id     = "bronze-tiering"
    status = "Enabled"

    filter {
      prefix = "bronze/"
    }

    transition {
      days          = 30
      storage_class = "STANDARD_IA"
    }

    transition {
      days          = 180
      storage_class = "GLACIER_IR"
    }

    noncurrent_version_expiration {
      noncurrent_days = 1
    }
  }

  rule {
    id     = "noncurrent-expiry"
    status = "Enabled"
    filter {}

    noncurrent_version_expiration {
      noncurrent_days = 30
    }
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "data_room" {
  bucket = aws_s3_bucket.data_room.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_public_access_block" "data_room" {
  bucket                  = aws_s3_bucket.data_room.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

# ---------------------------------------------------------------------------
# IAM: data-room-host instance role
# ---------------------------------------------------------------------------
#
# The host needs to: read/write the data-room bucket (and nothing else in
# S3), pull its two images from ECR, push the root-disk metric to
# CloudWatch, and be SSM-managed like the other hosts. No static keys.

resource "aws_iam_role" "data_room_host" {
  name               = "${var.project}-data-room-host"
  assume_role_policy = data.aws_iam_policy_document.ec2_assume.json
}

# Same managed policy the protocol hosts attach for SSM (see iam.tf).
resource "aws_iam_role_policy_attachment" "data_room_host_ssm" {
  role       = aws_iam_role.data_room_host.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "data_room_host_inline" {
  # RW on the data-room bucket ONLY.
  statement {
    actions   = ["s3:ListBucket"]
    resources = [aws_s3_bucket.data_room.arn]
  }
  statement {
    actions = [
      "s3:GetObject",
      "s3:PutObject",
      "s3:DeleteObject",
    ]
    resources = ["${aws_s3_bucket.data_room.arn}/*"]
  }

  # ECR pull, scoped to the two data-room repos (declared in ecr.tf).
  statement {
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }
  statement {
    actions = [
      "ecr:BatchCheckLayerAvailability",
      "ecr:GetDownloadUrlForLayer",
      "ecr:BatchGetImage",
    ]
    resources = [
      aws_ecr_repository.svc["data-room-collector"].arn,
      aws_ecr_repository.svc["data-room-batch"].arn,
    ]
  }

  # Root-disk metric cron (see user_data below). PutMetricData has no
  # resource-level scoping; constrain it to our namespace instead.
  statement {
    actions   = ["cloudwatch:PutMetricData"]
    resources = ["*"]
    condition {
      test     = "StringEquals"
      variable = "cloudwatch:namespace"
      values   = [local.data_room_metric_namespace]
    }
  }
}

resource "aws_iam_role_policy" "data_room_host_inline" {
  name   = "${var.project}-data-room-host-inline"
  role   = aws_iam_role.data_room_host.id
  policy = data.aws_iam_policy_document.data_room_host_inline.json
}

resource "aws_iam_instance_profile" "data_room_host" {
  name = "${var.project}-data-room-host"
  role = aws_iam_role.data_room_host.name
}

# ---------------------------------------------------------------------------
# Security group
# ---------------------------------------------------------------------------
#
# All egress; the only ingress is the node-exporter port (9100) from the
# shared services SG, where the central Prometheus runs — same
# source-SG-referencing pattern as ALB→EC2 and EC2→RDS. Everything else
# (deploys, shell) goes over SSM.

resource "aws_security_group" "data_room" {
  name        = "${var.project}-data-room"
  description = "data-room host. Egress-only, plus Prometheus scrape of node-exporter from the monitoring host SG."
  vpc_id      = aws_vpc.main.id

  ingress {
    description     = "node-exporter scrape from the shared host (central Prometheus)"
    from_port       = 9100
    to_port         = 9100
    protocol        = "tcp"
    security_groups = [aws_security_group.ec2.id]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

# ---------------------------------------------------------------------------
# EC2: data-room-host
# ---------------------------------------------------------------------------

variable "data_room_instance_type" {
  description = "EC2 instance type for the data-room host. t3.small is enough: the collector is IO-light and batch jobs are bounded; isolation from the protocol hosts is the point, not headroom."
  type        = string
  default     = "t3.medium"
}

variable "data_room_root_volume_gb" {
  description = "EBS gp3 root volume size in GB for the data-room host. 100 from day one — backfill unzips and spool churn are exactly the disk profile that filled options-host. EBS volumes can't shrink, so lowering this makes apply fail."
  type        = number
  default     = 100
}

locals {
  data_room_metric_namespace = "${var.project}/data-room"
  data_room_host_name        = "${var.project}-data-room-host"

  # Minimal first-boot script: docker + compose plugin, AWS CLI (for the
  # disk-metric cron), SSM agent, /opt/data-room, and the cron itself.
  # Deliberately NOT the shared cloud-init template machinery — no compose
  # files, no gatus/monitoring stack, no secrets rendering.
  data_room_user_data = <<-EOT
    #!/usr/bin/env bash
    set -euxo pipefail
    export DEBIAN_FRONTEND=noninteractive

    apt-get update
    apt-get install -y --no-install-recommends ca-certificates curl gnupg unzip jq

    # ---- Docker + compose plugin (same install as ec2-bootstrap.sh) ----
    if ! command -v docker >/dev/null 2>&1; then
      install -m 0755 -d /etc/apt/keyrings
      curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
        | gpg --dearmor -o /etc/apt/keyrings/docker.gpg
      chmod a+r /etc/apt/keyrings/docker.gpg
      ARCH=$(dpkg --print-architecture)
      CODENAME=$(. /etc/os-release && echo "$VERSION_CODENAME")
      echo "deb [arch=$ARCH signed-by=/etc/apt/keyrings/docker.gpg] \
        https://download.docker.com/linux/ubuntu $CODENAME stable" \
        > /etc/apt/sources.list.d/docker.list
      apt-get update
      apt-get install -y docker-ce docker-ce-cli containerd.io \
                         docker-buildx-plugin docker-compose-plugin
      systemctl enable --now docker
    fi

    # ---- AWS CLI v2 (disk-metric cron + ECR login) ----
    if ! command -v aws >/dev/null 2>&1; then
      curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" \
        -o /tmp/awscliv2.zip
      unzip -q /tmp/awscliv2.zip -d /tmp
      /tmp/aws/install
      rm -rf /tmp/aws /tmp/awscliv2.zip
    fi

    # ---- SSM Agent (Ubuntu ships it as a snap) ----
    if ! systemctl is-active --quiet amazon-ssm-agent 2>/dev/null; then
      snap install amazon-ssm-agent --classic || true
      systemctl enable --now snap.amazon-ssm-agent.amazon-ssm-agent.service || true
    fi

    mkdir -p /opt/data-room

    # ---- Root-disk used% -> CloudWatch every 5 minutes ----
    # No CloudWatch agent on any host in this stack; a one-line cron is the
    # whole telemetry path for the >=80% disk alarm (the options-host
    # containerd-fill lesson, encoded). df prints e.g. " 45%"; tr strips to
    # the bare number for put-metric-data. The cron line itself contains no
    # literal "%" (cron would treat it as a newline).
    cat > /etc/cron.d/data-room-disk-metric <<'CRON'
    */5 * * * * root used=$(df --output=pcent / | tail -1 | tr -dc 0-9); aws cloudwatch put-metric-data --region ${var.aws_region} --namespace ${local.data_room_metric_namespace} --metric-name RootVolumeUsedPercent --unit Percent --value "$used" --dimensions Host=${local.data_room_host_name}
    CRON
    chmod 644 /etc/cron.d/data-room-disk-metric
  EOT
}

resource "aws_instance" "data_room_host" {
  ami                         = var.host_ami
  instance_type               = var.data_room_instance_type
  subnet_id                   = aws_subnet.public[0].id
  vpc_security_group_ids      = [aws_security_group.data_room.id]
  iam_instance_profile        = aws_iam_instance_profile.data_room_host.name
  associate_public_ip_address = true
  key_name                    = var.ssh_pubkey == "" ? null : aws_key_pair.host[0].key_name

  user_data_base64            = base64gzip(local.data_room_user_data)
  user_data_replace_on_change = false

  root_block_device {
    volume_size = var.data_room_root_volume_gb
    volume_type = "gp3"
    # Keep the volume around if the instance is ever terminated — same
    # stance as the prod host (the bronze spool may hold not-yet-uploaded
    # capture).
    delete_on_termination = false
    encrypted             = true
  }

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required" # IMDSv2 only
    http_put_response_hop_limit = 2          # docker reaches the IMDS
  }

  # Same rationale as the other hosts: cloud-init runs user_data once at
  # first boot, so terraform should not try to push changes to a running
  # instance.
  lifecycle {
    ignore_changes = [user_data_base64]
  }

  tags = {
    Name = local.data_room_host_name
  }
}

# ---------------------------------------------------------------------------
# CloudWatch: root-disk alarm
# ---------------------------------------------------------------------------

resource "aws_cloudwatch_metric_alarm" "data_room_disk" {
  alarm_name          = "${var.project}-data-room-disk"
  alarm_description   = "data-room-host root volume >= 80% full. Disk fill wedged options-host (containerd snapshots -> SSM/cloud-init dead); catch it early here."
  namespace           = local.data_room_metric_namespace
  metric_name         = "RootVolumeUsedPercent"
  dimensions          = { Host = local.data_room_host_name }
  statistic           = "Maximum"
  period              = 300
  evaluation_periods  = 3
  comparison_operator = "GreaterThanOrEqualToThreshold"
  threshold           = 80
  # The metric arriving IS the host-liveness signal (cron + IMDS + AWS CLI
  # all working). Missing data means the host or cron is wedged — exactly
  # the failure mode this alarm exists for — so treat it as breaching.
  treat_missing_data = "breaching"

  # No alarm_actions: this root has no SNS topic (Grafana alerting handles
  # notifications today, and this metric never transits Prometheus). Wire an
  # SNS topic + subscription here if/when unattended paging is wanted.
}

# ---------------------------------------------------------------------------
# Outputs
# ---------------------------------------------------------------------------

output "data_room_bucket" {
  description = "S3 bucket holding the data-room market-data lake (bronze/silver/gold)."
  value       = aws_s3_bucket.data_room.bucket
}

output "data_room_instance_id" {
  description = "data-room-host instance ID, for the Deploy data-room workflow's SSM RunCommand."
  value       = aws_instance.data_room_host.id
}
