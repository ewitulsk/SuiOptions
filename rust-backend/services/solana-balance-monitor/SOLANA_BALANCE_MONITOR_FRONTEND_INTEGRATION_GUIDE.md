# solana-balance-monitor — frontend integration

Not frontend-facing. This service only exports Prometheus gauges (`sol_balance_sol`, `sol_balance_low`) and low-balance alert logs on ops port 9012 (`/health`, `/metrics`); no frontend integration exists or is planned.
