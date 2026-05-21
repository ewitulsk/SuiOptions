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
