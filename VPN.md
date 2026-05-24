# VPN access

We use [Tailscale](https://tailscale.com/) to reach AWS-internal services
(RDS Postgres, anything else inside the VPC) from your laptop. There's no
classical VPN client — Tailscale is a userspace daemon you sign into once
and forget about.

Under the hood: our EC2 host (`options-router`) advertises the VPC CIDR
`10.40.0.0/16` onto the tailnet. Any device on the tailnet that has
"accept routes" enabled can then talk to anything in the VPC, including
RDS.

## One-time setup

### 1. Get invited to the tailnet

Ask Evan to invite you via **Users → Invite users** in the Tailscale
admin console. Tailscale will email you a join link.

### 2. Install the client

```bash
brew install --cask tailscale
```

(Or grab the installer from <https://tailscale.com/download/macos>.)

On Linux/Windows: see <https://tailscale.com/download>.

### 3. Sign in and enable subnet routes

Open the Tailscale menu-bar app and sign in with the account that
received the invite. Then run:

```bash
tailscale set --accept-routes
```

Without this, your laptop will ignore the VPC subnet route and you
won't be able to reach RDS. This is per-device and persists.

### 4. Sanity check

```bash
tailscale status
```

You should see at least your own machine and `options-router` in the
list. If `options-router` isn't there yet, the EC2 hasn't registered —
ping Evan.

## Connecting to RDS

Once you're on the tailnet with `--accept-routes` enabled, the RDS
endpoint just works — DNS resolves it to its private IP, and the
Tailscale subnet route carries the traffic in.

```bash
# Pull the master password from Secrets Manager (needs your AWS creds).
PGPASSWORD=$(aws secretsmanager get-secret-value \
  --secret-id options/_master/db \
  --query SecretString --output text | jq -r .password)

psql -h options-db.cwn4a0i02lwq.us-east-1.rds.amazonaws.com \
     -U postgres \
     -d postgres
```

Per-env credentials live at `options/<env>/indexer` if you don't want
to use the master account:

```bash
PGPASSWORD=$(aws secretsmanager get-secret-value \
  --secret-id options/staging/indexer \
  --query SecretString --output text | jq -r .db_password) \
  psql -h options-db.cwn4a0i02lwq.us-east-1.rds.amazonaws.com \
       -U indexer_staging -d indexer_staging
```

GUI clients (TablePlus, DBeaver, DataGrip) work the same way — just
point them at the RDS hostname, username, and password.

## Troubleshooting

**`psql: connection to server timed out`**
The route isn't reaching you. Check:
1. `tailscale status` — is `options-router` listed and online?
2. `tailscale netcheck` — are you actually on the tailnet?
3. Did you run `tailscale set --accept-routes`? Verify with
   `tailscale debug prefs | grep RouteAll` — should be `true`.

**`tailscale status` doesn't show `options-router`**
Either the auth key hasn't been populated in Secrets Manager yet, or
the subnet route hasn't been approved in the admin console. Both are
operator-side; ping Evan.

**`could not translate host name "options-db..."`**
DNS resolution problem, not a routing problem. The RDS endpoint
resolves via *public* DNS even though the IP is private — so your
laptop needs working internet DNS. Try `dig options-db.cwn4a0i02lwq.us-east-1.rds.amazonaws.com`;
you should get back something in `10.40.x.x`.

**Connection works but is slow / drops**
Tailscale may be falling back to its relay (DERP) instead of going
direct. `tailscale ping options-router` will tell you — `direct` is
healthy, `via DERP` is functional but slower. Usually means a NAT
issue on your network; not actionable from your end.

## Cost / quota note

We're on Tailscale's free plan: up to 3 users and 100 devices. If we
outgrow that, we'll need to migrate to the Personal Pro or Team plan.
