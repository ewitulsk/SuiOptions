# Monthly spend estimate

Computed from current AWS public pricing (us-east-1, May 2026) against
exactly what's in this Terraform. One assumption everywhere: light
early-stage traffic. The variable line items (ALB LCU, ECR storage,
data transfer) are called out in §3 so you can re-estimate as traffic
grows.

## 1. Steady-state baseline

| Line item | Resource | Unit price | Quantity | Monthly |
|---|---|---|---|---|
| **Compute** | | | | |
| EC2 host | `t4g.medium` Linux on-demand, us-east-1 | $0.0336 / hr | 730 hr | **$24.53** |
| EBS root volume | gp3, encrypted | $0.08 / GB-mo | 30 GB | **$2.40** |
| **Database** | | | | |
| RDS Postgres | `db.t4g.micro`, single-AZ, on-demand | $0.016 / hr | 730 hr | **$11.68** |
| RDS storage | gp3 | $0.115 / GB-mo | 20 GB | **$2.30** |
| RDS backups | free up to 100% of allocated storage | — | 7-day retention, well under 20 GB | **$0.00** |
| **Ingress** | | | | |
| ALB hours | Application Load Balancer | $0.0225 / hr | 730 hr | **$16.43** |
| ALB LCU | rounded for light traffic (see §3) | $0.008 / LCU-hr | ~3 LCU-hr/day | **~$3.00** |
| ACM cert | public certificate | free | 1 | **$0.00** |
| **Secrets** | | | | |
| Secrets Manager entries | one master + 3 indexer + 2 mm-bot = 6 | $0.40 / secret-mo | 6 | **$2.40** |
| Secrets Manager API | ~30 GetSecretValue / day (one per deploy + a couple) | $0.05 / 10k calls | ~1k/mo | **$0.01** |
| **Registry** | | | | |
| ECR storage | 3 repos × ~20 tags × ~200 MB ARM image ≈ 12 GB; 500 MB free | $0.10 / GB-mo | ~11.5 GB | **$1.15** |
| ECR data transfer | EC2 pulls in same region | free | — | **$0.00** |
| **DNS** | | | | |
| Route 53 hosted zone | one zone, only if you create it here | $0.50 / zone-mo | 0–1 | **$0.00–0.50** |
| Route 53 queries | first 1B/mo at $0.40 / million | $0.40 / 1M | < 1M/mo | **~$0.04** |
| **Logging / state** | | | | |
| S3 (SSM output bucket) | 14-day lifecycle, ~KB-sized objects | $0.023 / GB-mo | < 1 GB | **<$0.10** |
| **Monitoring** (runs on the services EC2) | | | | |
| S3 Loki storage | log chunks + index, 90-day retention | $0.023 / GB-mo | ~5 GB (grows) | **~$0.12** |
| Secrets Manager | Grafana admin password (1 entry) | $0.40 / secret-mo | 1 | **$0.40** |
| **Networking** | | | | |
| VPC / IGW / subnets / route tables / SGs | | free | — | **$0.00** |
| NAT Gateway | not provisioned (EC2 in public subnet) | $0.045 / hr if used | 0 | **$0.00** |
| Data transfer out to internet | first 100 GB/mo free since 2024 | $0.09 / GB above 100 GB | well under 100 GB | **$0.00** |
| **IAM / KMS** | | | | |
| OIDC provider + roles | | free | — | **$0.00** |
| KMS (AWS-managed keys for SecretsManager + EBS) | | free | — | **$0.00** |
| **GitHub** | | | | |
| Actions minutes | free 2,000 min/mo on private repos; ~10 min/build × 2 builds/day | free at this volume | — | **$0.00** |

### Steady-state total

**≈ $64.56/mo** (with the Route 53 zone in your AWS account; **≈ $64.06/mo** without).
Includes the Grafana + Loki monitoring stack (~$0.52/mo added — no extra EC2).

## 2. One-time / occasional

| Item | Cost | Notes |
|---|---|---|
| Domain registration | varies by TLD; `.com` ~$13/yr via Route 53 | only if you don't already have the domain |
| First-deploy data transfer | a few GB out for cert validation + initial pulls | inside the 100 GB free tier |

## 3. What scales up first

These line items move as you grow. Watch them.

- **ALB LCU.** $0.008 per LCU-hour. Today's estimate (~$3/mo) assumes ≤25
  new WS connections/sec, ≤3,000 concurrent connections, <1 GB/hr through
  the LB, and minimal rule evaluations. The first dimension you'll exceed
  is likely **active connections** if many MM bots or frontend tabs stay
  connected. At 6,000 concurrent → 2 LCU-hr/hr → $11.68/mo just for LCU.
  Still small.
- **Data transfer out.** First 100 GB/mo free, then $0.09/GB. WebSocket
  streams from quoting-service to retail clients are the biggest mover.
  Rough math: a constant 1 MB/s outbound for 30 days = 2.6 TB → $234/mo.
  Almost certainly the first line item to swamp everything else.
- **ECR storage.** Each successful deploy pushes ~200 MB of new image
  layers across the three services. The lifecycle policy keeps last 20
  tags, so at high deploy cadence you'll hover near ~12 GB (~$1.15/mo).
  Costs grow linearly with tag retention if you raise it.
- **CloudWatch Logs** (when you turn it on per §13 of deployment.md).
  $0.50/GB ingested, $0.03/GB stored. The indexer at debug level can
  produce ~1 GB/day. Plan for ~$15-25/mo when you migrate to Loki or CW.
- **GitHub Actions minutes.** Free tier (2000/mo) covers ~200 builds.
  An ARM cross-build via QEMU takes 8-10 min. If you blow past that,
  $0.008/min for private repos. Switch to GH-hosted ARM runners or
  self-hosted runners to get back to free tier at high cadence.

## 4. If you turn off the cheap-tier shortcuts later

For comparison, the more-expensive baseline I quoted in early
brainstorming:

| Change | Monthly delta | New total |
|---|---|---|
| Switch RDS Postgres → Aurora `db.t4g.medium` single-AZ | +$36 | $100 |
| Add NAT Gateway (move EC2 to private subnet) | +$32 | $132 |
| Bump EC2 → `t3.medium` x86 (4 GB) | +$3 | $135 |
| Split prod onto its own EC2 (scaling plan A) | +$25-30 | $165 |
| Split prod onto its own Aurora cluster (scaling plan B) | +$45-50 | $215 |

Anchor: today's ~$65/mo gets you fully working staging + prod
stacks, all behind HTTPS, with managed Postgres and centralized
Grafana + Loki logging on a single EC2. Scaling plans add cost only when you actually
need them.

## 5. Sources

Prices pulled May 2026.

- [EC2 t4g.medium hourly](https://aws.amazon.com/ec2/pricing/on-demand/) ($0.0336/hr us-east-1)
- [RDS PostgreSQL pricing](https://aws.amazon.com/rds/postgresql/pricing/) (db.t4g.micro $0.016/hr single-AZ)
- [Elastic Load Balancing pricing](https://aws.amazon.com/elasticloadbalancing/pricing/) ($0.0225/hr + $0.008/LCU-hr)
- [Secrets Manager pricing](https://aws.amazon.com/secrets-manager/pricing/) ($0.40/secret-mo + $0.05/10k API calls)
- [ECR pricing](https://aws.amazon.com/ecr/pricing/) ($0.10/GB-mo storage; 500 MB free for private repos)
- [Route 53 pricing](https://aws.amazon.com/route53/pricing/) ($0.50/zone-mo, $0.40/million queries; alias records to ALB are free)
- [EBS pricing](https://aws.amazon.com/ebs/pricing/) (gp3 $0.08/GB-mo)
- [Data transfer pricing](https://aws.amazon.com/ec2/pricing/on-demand/#Data_Transfer) (first 100 GB/mo to internet free)
