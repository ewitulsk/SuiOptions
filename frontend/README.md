# Tideline Frontend

Aqua-themed UI for the SuiOptions protocol. MVP scaffold — UI shell is wired,
data is mocked and ready to swap for real Sui SDK calls.

## Stack

- Vite + React 18 + TypeScript
- Pure CSS (`src/styles/aqua.css`) — the design system, ported verbatim from the
  Claude Design handoff. Each screen is themed via `[data-theme="aqua"]`.
- Google Fonts: `Sora` (display) + `JetBrains Mono` (numerals/labels).

## Where to plug in real data

All UI components consume plain data + callbacks. The mock state hooks in
`src/mocks/` are the seams to replace:

| File                     | Replace with                                          |
| ------------------------ | ----------------------------------------------------- |
| `mocks/composer.ts`      | Live spot feed, MM quote stream, submit-quote tx flow |
| `mocks/dashboard.ts`     | On-chain positions + bucket cursors + exercise/claim  |
| `mocks/activity.ts`      | Indexer-backed event log + WSS subscription           |

Each hook returns the same shape as its UI consumers expect, so swapping the
implementation to real Sui SDK / indexer / WSS calls is a contained change.

## Scripts

- `npm install`
- `npm run dev` — Vite dev server on :5173
- `npm run build` — type-check + production build
- `npm run typecheck`

## Environment variables

The frontend reads a handful of `VITE_`-prefixed variables. **All have defaults
wired for local dev (or are optional), so none are strictly required** — set
them to point at a non-local environment or to enable social sign-in. Define
them in `frontend/.env.local` (Vite loads it automatically; not committed).

| Variable               | Required | Default                  | Purpose |
| ---------------------- | -------- | ------------------------ | ------- |
| `VITE_ENVIRONMENT`     | No       | `testnet`                | Selects which deployment block to read from `rust-backend/deployments.json`. One of `mainnet` \| `testnet` \| `devnet`. Drives the package / protocol-config / treasury ids (`src/config.ts`) **and** the wallet's default Sui network (`src/main.tsx`). An environment with no published deployment leaves those ids unset and the app falls back to its empty / "no deployment configured" states. |
| `VITE_API_BASE_URL`    | No       | `http://127.0.0.1:9003`  | Base URL of the Rust `api-service` (REST). |
| `VITE_QUOTING_WS_URL`  | No       | `ws://127.0.0.1:9002/`   | WebSocket URL of the quoting service. |
| `VITE_ENOKI_API_KEY`   | No       | _(unset)_                | Enoki **public** frontend API key (Enoki Developer Portal). Enables zkLogin "Sign in with Google" in the connect modal. When unset, social login is hidden and only browser-extension wallets are offered. |
| `VITE_GOOGLE_CLIENT_ID`| No       | _(unset)_                | Google OAuth 2.0 **web** client id. Required alongside `VITE_ENOKI_API_KEY` to register the Enoki Google wallet. Both must be set for social login to appear. Enoki only supports `mainnet`/`testnet` (not `devnet`). |

Contract ids (package, `ProtocolConfig`, `Treasury`) are **not** env vars —
they come from `deployments.json` keyed by `VITE_ENVIRONMENT`.

Example `frontend/.env.local`:

```
VITE_ENVIRONMENT=testnet
VITE_API_BASE_URL=https://api.staging.example.com
VITE_QUOTING_WS_URL=wss://quotes.staging.example.com/
```
