# solana-oracle-service — Frontend Integration Guide

**Frontends do not call this service.** It is internal-only (port 9013 on the
docker network, no nginx route — same posture as the Sui oracle-service).
Frontend price/vol needs are served by the public services
(solana-api-service, solana-price-charting), which read through
`crates/oracle-client` internally.

## Endpoints (internal consumers only)

Base URL inside the docker network: `http://solana-oracle-service:9013`.
Backend consumers should use `crates/oracle-client` rather than raw HTTP.

| Endpoint | Description |
|---|---|
| `GET /prices` | Latest cached price for every discovered feed (`as_of_ms`, `upstream_healthy`, `prices[]` of `{feed_id, price, conf, publish_time_ms, age_ms}`). `/snapshot` is an alias. |
| `GET /prices/:feed` | Single feed by 64-hex Pyth feed id. `400` malformed id, `404` not cached yet. |
| `GET /vol/realized?feeds=<hex,hex>&window_days=<n>` | Cached realized vol from Pyth Benchmarks; beta→stable feed-id mapping is applied service-side. |
| `GET /ws` | WebSocket fanout: a `status` frame on connect, then every price update as it streams in from Hermes. |
| `GET /health`, `GET /metrics` | Ops only (Gatus / Prometheus). |

## Notes

- Feeds are discovered at boot from the solana-token-info catalog (every
  token with a `pyth_feed_id`). Pyth feed ids are chain-agnostic — the same
  gateway engine as the Sui oracle-service, pointed at the Solana catalog.
- Currently on **hermes-beta** (beta feed id set); mainnet cutover to stable
  Hermes is a config-only change.
