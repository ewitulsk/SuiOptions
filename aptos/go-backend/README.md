# aptos/go-backend

Planned services (see `docs/aptos-nft-marketplace/00-plan.md` §4.3, §6):

- `cmd/nft-indexer` — Transaction Stream (gRPC) processor into Postgres: our venue + every foreign venue + token metadata.
- `cmd/nft-api` — public REST/WS and JWT-gated admin mux; builds buy/list/offer payloads (tier 0 direct, tier 1 router).

Separate Go module from `../../go-backend`; platform packages (config, db, obs, cors) are copied, not imported.
