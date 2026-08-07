# exchange — hybrid exchange settlement package

Atomic on-chain settlement for the hybrid exchange (off-chain orderbook,
maker-signed limit orders), modeled on 0x v4 Limit Orders. The off-chain
half is `rust-backend/services/orderbook` plus the `exchange-*` crates.

## Audit boundary

Deliberately **framework-only dependencies** (the `auction` package
pattern): no options-protocol code is in scope, and no options package may
depend on this one. Future integrations (cross-margin, vault fills) belong
in a separate adapter package.

## Modules

| module | contents |
|---|---|
| `order.move` | Order struct + BCS decode, domain-separated blake2b digest (tag ‖ version ‖ registry ID), ed25519/secp256k1 (low-s) signature verification, canonical type strings. Consensus-critical mirror of the Rust `exchange-signing` crate. |
| `balance_manager.move` | Per-user shared escrow (the ERC-20-allowance replacement). Package-gated debit/credit; withdrawal is owner-only, instant, and independent of pause state. Delegated signer set (≤16). |
| `registry.move` | Per-market `SettlementRegistry<Base, Quote>`: digest-keyed fill accounting, salt watermarks, pause, fee config under a hard-coded 50 bps ceiling, fee vaults, permissionless `gc`. |
| `settlement.move` | `fill_limit_order` (+ reverse), `match_orders` (resting-price execution, both signed limits enforced), `cancel` / `cancel_up_to`, `assert_coin_min` route guard. One abort code per check, in check order — the relayer decodes them. |
| `fees.move` / `admin.move` | Vault sweeps; `AdminCap` (pause, fees, listing — no admin path touches user escrow or fill state). |

## Consensus-critical invariants

- `Order` field order / BCS layout never changes without bumping
  `DOMAIN_VERSION` and regenerating the cross-language conformance fixtures
  (`tests/conformance_tests.move` ↔
  `rust-backend/crates/exchange-signing/fixtures/conformance.json`). Both
  suites are release-blocking.
- Signature recipe (both schemes): sign
  `blake2b256(intent ‖ bcs(digest))`; the secp256k1 native sha256-hashes
  internally on top. This matches Sui wallets' `signPersonalMessage`.
- The registry **object ID** is the signature domain. Consequences: a
  package republish forks the exchange (old registries stay bound to the
  old package) — hence `deployment-manager --deploy-exchange` is an
  explicit ceremony, never part of the default protocol publish — and
  package *upgrades* do NOT invalidate outstanding orders, which must be
  weighed in every upgrade review.

## Deploy

```sh
# publish (records package/upgradeCap/adminCap under the env's `exchange`
# block in deployments.json; markets map is filled by the market-creation
# ceremony via the exchange AdminCap)
cd rust-backend && cargo run -p deployment-manager -- \
  --env staging --network testnet --deploy-exchange
```

CI: `move-ci.yml` matrix builds+tests this package;
`deployment-manager/tests/deploy_build.rs` gates it with the exact deploy
compiler (SO-335).
