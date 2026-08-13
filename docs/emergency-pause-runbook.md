# Emergency pause runbook

**Scope:** stopping money ingress protocol-wide (options writes, trading-vault
deposits + vault creation, exchange deposits + fills) in one command, deciding
when to, and unwinding it. Exits are never gated — see the invariant below.

Companion docs: `docs/fund-egress-audit.md` (where money rests and how it
exits), the guarded-launch whitelist (SO-381 epic, PRs #449–#453).

---

## 0. The invariant this design rests on

**Ingress is gated; exits never are.** `exercise`, `redeem_position`,
`request_withdraw` + fulfillment cranks, BalanceManager `withdraw`, order
`cancel*`, `return_external`, and every force/crank session keep working for
everyone — non-members, de-listed members, and while paused. Consequences:

- Pausing can never strand customer funds. There is no state where a user
  cannot get out. You can therefore pause EARLY and cheaply — a false-positive
  pause costs minutes of ingress, not customer trust.
- The reverse consequence: **a pause does not stop an exploit that has already
  minted a claim.** If a bug lets someone fabricate a position/withdrawal
  claim, the exit path will honor it. Pause buys time by stopping NEW exposure
  only. Detection latency is the whole game — hence the balance-monitor drain
  watch.

## 1. Detection — what should page you

| alert_id | Source | Meaning |
|---|---|---|
| `drain-suspected-<kind>` | balance-monitor protocol-holdings watch | A watched pool (bucket escrow / vault holdings / exchange BM) dropped faster than the configured bps/window |
| `whitelist-changed` | balance-monitor admin-change watch | Core or exchange whitelist membership/flags changed — expected only when YOU ran the CLI/UI |
| `protocol-paused` | balance-monitor admin-change watch | An ingress-pause flag flipped — expected only when you flipped it |
| `low-balance-*`, `tx-failed-*` | existing rules | Standard service alerts; a burst can be secondary evidence |

`whitelist-changed` firing when nobody on the team ran an admin action means
the deployer key is compromised — treat as a live incident, go to §2
immediately, then rotate.

## 2. The big red button

```
# from rust-backend, deployer key in the tool's secrets TOML; point
# --token-info-url at the target env's token-info (default localhost:9005 —
# SSM port-forward to the host, or run on the host itself):
cargo run -p exchange -- --secrets <deployer-secrets.toml> \
    --token-info-url <env token-info> pause-ingress
```

One PTB, three levers atomically:
1. the protocol's single `Whitelist.ingress_paused = true` — blocks option
   writes, vault deposits + creation, exchange BM deposits AND all fills
   (taker + relayer paths); every gated package checks this one object
2. trading-vault `registry::set_paused(true)` — belt-and-braces vault
   deposit stop
3. `exchange::registry::set_paused(true)` on EVERY market — belt-and-braces
   per-market fill stop

The same action is available from the dashboard admin page ("Ingress paused"
toggle) if you have the admin wallet in a browser.

## 3. Verify the pause took

- `cargo run -p exchange -- whitelist-list` (same global flags) → the
  whitelist shows `ingress_paused: true`.
- Grafana: `protocol-paused` alert fired (this is your audit trail that the
  flip was seen on-chain).
- Spot-check what still works: a `request_withdraw` + fulfillment crank on a
  staging vault, a BM `withdraw`. These MUST still succeed — if they don't,
  something else is wrong; escalate past this runbook.

## 4. Decide + communicate

- If drain suspected: identify the outflow from the balance-monitor gauges
  (`protocol_holdings{object,kind}`) and recent txs of the affected object.
  Attribution: the whitelist means every ingress sender was a known member —
  pull the member list and match the tx sender.
- Post status (status.sui-options.com) if user-visible: "deposits and trading
  paused, withdrawals unaffected".

## 5. Additional levers (finer-grained than the big red button)

| Lever | Command / call | Blocks | Doesn't block |
|---|---|---|---|
| Remove one member | `exchange whitelist-remove --address 0x…` | that address's ingress | their exits |
| Per-bucket invalidate | core `invalidate_bucket` (existing admin surface) | new writes into one bucket | exercise/redeem |
| Per-vault deposits | curator `set_deposits_paused` | one vault's deposits | withdrawals |
| Adapter/oracle delist | `registry::disallow_adapter/oracle` | new sessions/attestations | force sessions, cranks |
| Stop services | stop-service(s) workflow | our own bots/relayer | direct chain access |

## 6. Unpause

Criteria: root cause understood, fix deployed or threat ruled out, holdings
reconciled against the monitor's pre-incident baseline.

```
cargo run -p exchange -- --secrets <deployer-secrets.toml> \
    --token-info-url <env token-info> unpause-ingress
```

Then verify: member deposit/write succeeds again; `whitelist-list` shows both
flags false; drain watch quiet for one full window.

## 7. Going public (not an emergency action)

`whitelist-disable` / `whitelist-enable` flip enforcement on the single
whitelist without touching membership. Disabling is the post-audit go-public
lever; re-enabling restores the prior cohort instantly. Never confuse with
pause: pause blocks members too.

## 8. Redeploy interaction

Contracts republish on every redeploy → a fresh Whitelist object, seeded by
the ceremony right after the whitelist package publish: deployer automatically
+ `INGRESS_MEMBERS` const +
`vars.INGRESS_MEMBERS_STAGING` (comma-separated service wallets: orderbook
relayer/staging-mm-bot wallet, mm-bot wallet). **If the repo variable is unset
or stale, bots fail their first gated call after redeploy** — the fix is
`whitelist-add` for the missing wallet, then update the variable.
