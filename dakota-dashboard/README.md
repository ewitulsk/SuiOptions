# dakota-dashboard

Console for the [Dakota](https://docs.dakota.xyz) stablecoin on/off-ramp
integration. Talks to `rust-backend/services/dakota-service` and
`rust-backend/services/auth-service`.

**Self-contained by design.** Nothing here is shared with `frontend/` — its own
`package.json`, `node_modules`, `tsconfig.json` and Vercel project. The two apps
happen to use the same tooling; they share no code.

## One app, four audiences

There is a single build. The JWT's `role` claim decides which routes render, and
`dakota-service` enforces the same boundary server-side — the UI never filters
data it was not already scoped out of.

| Role | Sees | Reached by |
|---|---|---|
| `admin` | The whole platform: assets, rates, every customer, ramps, treasury, ops | Sui wallet on the `admin_addresses` allowlist |
| `business` | Its own customers and their flows; can invite them | An invite minted by an admin |
| `individual` | Only itself | An invite minted by an admin **or** by the business it belongs to |

That last row is the point of the hierarchy: a partner business sends its own
customers a signup link, and those customers land in a console scoped to
themselves without us being involved.

## Auth

Username + password, or a Sui wallet, or both on one account. Settings → Security
adds the second method in either direction; either then signs you in.

No email is stored anywhere, so **there is no password reset** — recovery is an
admin minting a fresh invite. Accounts are only created by redeeming an invite;
the one exception is an allowlisted wallet, which bootstraps as an admin on
first login.

## Running locally

```sh
npm install
cp .env.example .env      # point at staging, or at local services
npm run dev               # http://localhost:5174
```

Port 5174 keeps it clear of the protocol frontend on 5173, and both dev origins
are already in the services' CORS allow-lists.

Against local services you also need `auth-service` and `dakota-service` running
with their databases created — see `rust-backend/services/dakota-service/config/config.toml`.

## Deploying

Its own Vercel project rooted at this directory. `vercel.json` carries the SPA
rewrite. Set `VITE_DAKOTA_API` and `VITE_AUTH_API`, and add the deployment origin
to `allowed_origins` in both services' staging configs.

**Staging only.** `dakota-service` integrates Dakota's *sandbox* and is
deliberately absent from the prod compose file, so there is nothing for a
production build of this app to talk to.

## Sandbox limits worth knowing

- **$2.00 per transaction.** Enforced in the forms and again server-side.
- **Testnets only** — the sandbox lists mainnet network ids and then rejects them.
- Banking is mocked, so onramps are funded with *Simulate a deposit* rather than a
  real wire. Crypto legs settle for real on testnets.
- A customer cannot open a ramp until Dakota approves them. In sandbox that is
  the **Approve** button on the Customers screen (`kyb_approve`, which is the
  transition that works for individuals too).
- Nothing appears in Flows until the webhook target is registered — do it once
  from Ops, and use **Resync** to backfill anything missed.
