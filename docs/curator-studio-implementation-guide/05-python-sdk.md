# 05 — P1: Python SDK (`curator-sdk`) + template bot

New top-level dir `python-sdk/` (repo has no Python precedent; this sets it). The SDK is a **typed thin client over bot-gateway** — no PTB logic, no chain access, no keys (spec D4/D11). Simple enough that a cheap model drives it correctly; strict enough that wrong code fails mypy.

```
python-sdk/
  pyproject.toml            name = "curator-sdk"; py>=3.11; deps: httpx, pydantic>=2, websockets
  curator_sdk/
    __init__.py
    client.py               GatewayClient (sync httpx; bots are simple loops — no asyncio in v1)
    models.py               pydantic: Spec, RiskLimits, Market, Book, Order, Fill, VaultState, Price
    orders.py               OrderBuilder — REQUIRES a RiskLimits instance; no bypass constructor
    strategy.py             Strategy protocol + StrategyContext
    primitives/             covered_call_writer.py, cash_secured_put_writer.py,
                            delta_band_rebalancer.py, passive_premium_seller.py
    runtime.py              the scaffolding loop (heartbeat, control, funds-wait, paper|live)
    testing.py              test-runner client (P2): submit_test, get_report
    keys.py                 unwrap_exported_key (P4)
  tests/                    pytest + respx fixtures against recorded gateway responses
  mypy.ini                  strict = True; disallow_any_explicit = True
```

## 1. Type-enforced risk invariants

The type system encodes the rule "no order without limits" (spec §9):

```python
class RiskLimits(BaseModel, frozen=True):
    max_notional_per_epoch: int
    max_open_contracts: int
    price_band_bps: int
    markets: tuple[str, ...]
    stop_loss_drawdown_pct: int

    @classmethod
    def from_spec(cls, spec: Spec) -> "RiskLimits": ...

class OrderBuilder:
    def __init__(self, market: Market, limits: RiskLimits): ...   # ONLY constructor
    def limit(self, side: Side, price: Decimal, lots: int, ttl_s: int = 90) -> OrderIntent: ...
```

`GatewayClient.place(intent: OrderIntent)` is the only submission path and `OrderIntent` is only produced by `OrderBuilder`. The gateway re-validates everything anyway (defense in depth); the types exist so generated code fails fast and locally.

## 2. Strategy protocol

```python
class StrategyContext(Protocol):
    spec: Spec
    def book(self, market: str) -> Book: ...
    def price(self, feed: str) -> Price: ...
    def vault(self) -> VaultState: ...
    def orders(self) -> OrderBuilder factory ...
    def place(self, intent: OrderIntent) -> PlaceResult: ...
    def cancel_all(self) -> None: ...
    def log(self, msg: str, **fields: str | int | float) -> None: ...

class Strategy(Protocol):
    def on_tick(self, ctx: StrategyContext) -> None: ...          # called every refresh
    def on_fill(self, ctx: StrategyContext, fill: Fill) -> None: ...
```

Primitives implement `Strategy` and are constructed from the spec: `covered_call_writer.from_spec(spec)`. The four v1 primitives mirror spec §7.2; each ships with unit tests over synthetic `StrategyContext` fixtures and docstrings written **for the agent** (they are the material the quiz agent and future BYO skill read).

## 3. The template bot / runtime scaffolding

`runtime.py` is the non-negotiable scaffolding (spec D16) — strategy code plugs *into* it and cannot remove it:

```python
def run() -> None:
    cfg = RuntimeConfig.from_env()          # GATEWAY_URL, BOT_API_TOKEN, SPEC_JSON, VAULT_ID, BOT_MODE
    client = GatewayClient(cfg)
    spec = Spec.model_validate_json(cfg.spec_json)
    strategy = load_primitive(spec)         # v1: primitives only

    hb = Heartbeat(client, interval_s=15)   # separate thread; carries state + open-order count;
                                            # the RESPONSE carries control: run|pause|kill
    wait_for_funds(client)                  # poll vault state until deposits arrive
    while True:
        control = hb.control()
        if control == "kill":
            client.cancel_all(); break
        if control == "pause" or not client.prices_fresh():
            time.sleep(2); continue         # park, don't crash
        try:
            strategy.on_tick(ctx)
        except GatewayReject as e:
            log.warning("rejected: %s", e.code)   # typed 422s are data, not crashes
        except Exception:
            log.exception("strategy tick failed") # scaffolding survives strategy bugs
        time.sleep(spec.execution.refresh_s)
```

- **Control channel is the heartbeat response** — one HTTP round-trip carries liveness out and control in; the WS channel additionally pushes `pause`/`kill` for sub-interval latency. Kill always cancel-alls before exit; the gateway raises the watermark server-side regardless (bot cooperation is best-effort, the gateway is authoritative).
- **`BOT_MODE=paper`**: same strategy code; `GatewayClient` routes `place` to the gateway's paper engine (P2) which simulates fills against the live book; `vault()` returns the simulated portfolio.
- Logging: structured JSON lines to stdout (Fly captures them; `fly logs` is the v1 bot-log story, surfaced in the dashboard via Fly's API later).

`bot-runtime/entry.py` (chapter 04 §5) is literally `from curator_sdk.runtime import run; run()`.

## 4. mypy + CI

- `mypy --strict` on `curator_sdk` and on the template; CI job in the repo workflow (new `python-sdk.yml`: ruff + mypy + pytest on 3.11/3.12).
- The P4 bespoke pipeline reuses the same mypy config as its static gate (08 §1) — keeping SDK types strict *is* the product's code-quality gate, treat any `Any` escape as a bug.

## 5. Versioning & publishing

- v1: consumed only by the bot-runtime image (installed from the repo path).
- P4: publish to PyPI as `curator-sdk` (spec §9); template repo export bundles a pinned version. SemVer; the gateway carries an API version header and rejects SDKs below the floor.
