# aptos/

Aptos NFT marketplace + cross-venue aggregator. Sibling to the Sui options
stack; shares repo conventions and infra, not code.

- `contracts/` — Aptos Move packages (Aptos CLI toolchain, not `sui`).
- `go-backend/` — Go module for the indexer, API, and transaction-payload builders.

Plan and landscape research: [`docs/aptos-nft-marketplace/00-plan.md`](../docs/aptos-nft-marketplace/00-plan.md).
Nothing here is built yet; chapter 01 of the plan defines the build order.
