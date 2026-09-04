# aptos/contracts

Planned packages (see `docs/aptos-nft-marketplace/00-plan.md` §6):

- `marketplace/` — our venue, forked from `aptos-core/aptos-move/move-examples/marketplace`.
- `router/` — `buy_many` across venues with one adapter module per foreign marketplace.

Build/test with the Aptos CLI: `aptos move test --dev` per package. CI: `aptos-move-ci.yml` (to be added).
