# scraper/backend

Python service implementing plan §1–2: marketplace adapters (eBay Browse API first),
poll scheduler, two-stage AI valuation (LiteLLM — swap OpenAI/Claude/OpenRouter via
`TRIAGE_MODEL` / `FULL_MODEL` env vars), Discord deal alerts, auth service, and the
deals/P&L API with actual buy/sell tracking (`net_profit` is a DB-computed column).

## Layout

- `app/adapters/` — `MarketplaceAdapter` interface + eBay implementation
- `app/valuation/` — LiteLLM triage + full valuation (photos included)
- `app/pipeline/` — ingest → dedupe → valuate → alert
- `app/scheduler.py` + `app/worker.py` — poll loop (`python -m app.worker`)
- `app/auth/` — users, bcrypt, signed session cookies, seed admin
- `app/api/` — saved searches CRUD, listings feed, deals + P&L stats
- `app/main.py` — FastAPI app (API process)

## Run locally

```bash
cp .env.example .env   # fill in provider + eBay keys
uv sync
uv run uvicorn app.main:app --reload   # API on :8000
uv run python -m app.worker            # scraper loop (separate terminal)
```

Or the full stack (Postgres + api + worker): `docker compose up` from `scraper/`.

## Tests / lint

```bash
uv run pytest
uv run ruff check .
```

Tests run against SQLite with all network calls mocked — no keys needed.
