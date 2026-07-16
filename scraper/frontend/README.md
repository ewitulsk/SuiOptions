# scraper/frontend

Vite + React 18 + TypeScript dashboard (plan §3): login, deal feed with AI
valuations and copyable outreach drafts, saved-search CRUD, and the Deals/P&L
page — mark deals bought/sold with actual prices and watch net profit,
capital tied up, win rate, and per-user splits update.

## Run locally

```bash
npm install
npm run dev        # http://localhost:5173, proxies /api + /auth to :8000
```

Start the backend first (see `../backend/README.md`). Log in with the seeded
admin user (`SEED_ADMIN_PASSWORD`).

```bash
npm run build      # typecheck + production bundle to dist/
```
