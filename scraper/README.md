# scraper

Marketplace arbitrage bot: scrapes new listings from online marketplaces, values
them with a pluggable AI layer (OpenAI / Claude / OpenRouter), and alerts on
underpriced finds.

See [PLAN.md](PLAN.md) for the full implementation plan.

- `backend/` — Python scrapers, AI valuation pipeline, FastAPI (plan §1–2)
- `frontend/` — Vite + React + TS dashboard (plan §3)
- `infra/` — self-contained Terraform module + deployment bundle (plan §4)

Deployment workflows live at `.github/workflows/scraper-*.yml` (plan §5).
This folder is self-contained by design — see the portability contract in plan §4.
