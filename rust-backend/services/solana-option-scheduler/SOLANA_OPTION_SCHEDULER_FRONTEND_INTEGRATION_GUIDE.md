# solana-option-scheduler — frontend integration

This service is **not frontend-facing**: it is an internal bot that rolls option-bucket families (`create_bucket`) and auto-provisions covered-call vaults (`create_vault`) with the admin keypair.
Frontends never call it; they read the buckets/vaults it creates from the solana-indexer GraphQL API (`/{env}/solana-indexer/graphql`) and the solana-api-service.
Bucket/vault PDAs are deterministic (salt = sha256 of mints + expiry/round + strike terms — plus a replacement generation for vaults — first 8 bytes LE), so ids are stable across re-runs.
Ops surface only: `GET /health` and `GET /metrics` on port 8087.
