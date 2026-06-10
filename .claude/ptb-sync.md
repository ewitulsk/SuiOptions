# PTB sync invariant: frontend ↔ gas-station templates

**Rule: any change to a frontend PTB builder for a sponsored flow MUST update
the matching gas-station template AND its shape unit test in the same PR.**

The gas-station only sponsors PTBs that structurally match one of its
templates (`rust-backend/crates/sui-tx/src/tx/template.rs`). If the frontend
ships a new PTB shape without a matching template update, sponsorship
**silently refuses every transaction of that flow** — no chain error, the
user just can't transact. The two sides are deployed independently, so this
drift is invisible in single-service testing.

## Builder ↔ template map

| Frontend builder (`frontend/src/tx/`) | Template (`template.rs` name) | Move targets (required, in order) | Command shape |
|---|---|---|---|
| `composer.ts buildWriteTx` | `write` | `quote::new_quote` → `quote::new_signed_quote` → `bucket::execute_write_writer_flow` (3 type args) | `coinWithBalance` prelude (SplitCoins/MergeCoins) + trailing `TransferObjects` of the returned `(Position, Coin<Settlement>)` |
| `composer.ts buildBuyTx` | `buy` | `quote::new_quote` → `quote::new_signed_quote` → `bucket::execute_write_trader_flow` (3 type args) | `coinWithBalance` prelude + trailing `TransferObjects` of the returned `Coin<Call>` |
| `dashboard.ts buildExerciseTx` | `exercise` | `bucket::exercise` (3 type args) | `coinWithBalance` ×2 + `TransferObjects` of the returned underlying |
| `dashboard.ts buildRedeemTx` | `redeem` | `bucket::redeem_position` (3 type args) | `TransferObjects` of the two returned coins |
| `faucet.ts` mint | `faucet_mint:<module>` | `<tokens_pkg>::<module>::mint_to_sender` | dev/staging only |
| `deepbook.ts buildCreateVenueTx` | `deepbook_create_pool` | `pool::create_permissionless_pool` (2 type args) | `coinWithBalance` DEEP-fee prelude |
| `deepbook.ts` BM create | `deepbook_bm_create` | `balance_manager::new` → `register_balance_manager` → `0x2::transfer::public_share_object` | |
| `deepbook.ts` limit/market order | `deepbook_place_limit` / `deepbook_place_market` | optional `balance_manager::deposit`, `generate_proof_as_owner` → `pool::place_*_order` (2 type args) | |
| `deepbook.ts` cancels | `deepbook_cancel_order` / `deepbook_cancel_all` | proof → `pool::cancel_order` / `cancel_all_orders` | |
| `deepbook.ts` withdraw | `deepbook_withdraw` | proof → `pool::withdraw_settled_amounts` → `balance_manager::withdraw_all` ×2 | trailing `TransferObjects` |

Notes:
- Templates match (a) a **closed set** of allowed Move-call targets, (b)
  **type-arg arity** on anchor calls, and (c) the required targets as an
  **ordered subsequence**. Non-Move-call commands (`SplitCoins`,
  `MergeCoins`, `TransferObjects`, `MakeMoveVec`) are treated as benign —
  adding/removing those does NOT need a template change; adding/removing/
  renaming a **Move call** DOES.
- The Rust reference builders in `crates/sui-tx/src/tx/execute_write.rs`
  build the same write/buy shapes for `tools/writer` / `tools/trader` — keep
  them in sync too (they're the executable documentation of the shape).
- Admin/org PTBs (`tx/admin.ts`, `tx/org.ts`) are NOT sponsored — they're
  signed by cap-holders who pay their own gas — so they have no templates.

## How to verify

1. `cargo test -p sui-tx` — the template tests in `template.rs` construct
   the exact PTB shapes the frontend emits and assert they match (e.g.
   `write_flow_with_trailing_transfer_objects_matches`). Add/extend a test
   whenever a shape changes.
2. Smoke a sponsored tx on staging after deploying both sides: a write from
   the Earn screen with gas-station sponsorship enabled. A template mismatch
   shows up as the gas-station refusing sponsorship (check its logs for the
   "no template matched" line).
