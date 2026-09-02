# Data Room Runbook

Spec: `docs/data-room-spec.md` (repo root). Host: `options-data-room-host`
(SSM-managed, no SSH). Lake bucket: Terraform output `data_room_bucket`.
Everything below `/opt/data-room` on the host: `compose.yml`, `.env`,
`collector.toml`, systemd units (from `rust-backend/data-room/deploy/`).

## Build, test, images

The crates are members of the `rust-backend/` Cargo workspace (SO-449);
run cargo from `rust-backend/` and always scope with `-p` — a bare
`--workspace` compiles the Sui tree:

```bash
cd rust-backend
P="-p data-room-schema -p data-room-adapters -p data-room-store -p collector -p vision-sync -p normalizer -p gold"
cargo test $P
cargo clippy $P --all-targets -- -D warnings
cargo fmt $P            # data-room crates stay rustfmt-clean; never `cargo fmt` the rest of rust-backend
python3 data-room/ci/check_isolation.py   # no protocol / Sui / diesel deps from data-room
# images: context = rust-backend/ (here), scoped to the data-room members by scope-workspace.sh
docker buildx build . --file data-room/Dockerfile.collector -t data-room-collector:local
docker buildx build . --file data-room/Dockerfile.batch -t data-room-batch:local
```

## Daily operation (all automatic)

| When (UTC) | Unit | What |
|---|---|---|
| always | `collector` (compose, restart always) | Coinbase WS → bronze |
| 00:10 | `data-room-instruments` | instrument snapshot |
| 00:30 | `data-room-normalizer` | yesterday + 3-day lookback → silver |
| 01:30 | `data-room-gold` | gaps → bars → rv for yesterday |
| monthly | `data-room-vision-sync` | new Binance dumps → bronze + silver |

Alerts (Grafana, via Loki `alert_id`): `dataroom-collector-stalled`,
`dataroom-upload-stalled`, `dataroom-quality-gate`. Host disk metric:
CloudWatch `options/data-room` / `RootVolumeUsedPercent` (alarm ≥ 80 %).

## Restart / redeploy collector

```bash
# from GH Actions: run "Deploy data-room" (builds + SSM deploys), or by hand:
aws ssm start-session --target <instance-id>
cd /opt/data-room && docker compose pull && docker compose up -d collector
```

Restarts are safe at any time: the spool flushes on SIGTERM, leftover
files upload on next boot (boot sweep), and a capture hole becomes a
ledgered gap, not corruption.

## Replay a day (adapter bugfix, quality-gate failure)

Silver partitions are overwrite-idempotent; bronze is never touched.

```bash
docker compose run --rm batch normalizer coinbase --date 2026-08-12
# vision dumps: delete the state marker(s), then re-run
aws s3 rm s3://$BUCKET/silver/v1/_state/vision/BTCUSDC-trades-2026-08.zip.done
docker compose run --rm batch normalizer vision
# then regenerate gold for affected dates
docker compose run --rm batch gold --date 2026-08-12 gaps
docker compose run --rm batch gold --date 2026-08-12 bars
docker compose run --rm batch gold --date 2026-08-12 rv
```

## One-shot full Binance backfill (R3)

```bash
docker compose run --rm batch vision-sync            # spot: everything, ~5 GB
docker compose run --rm batch vision-sync --since 2024-01   # or bounded
docker compose run --rm batch vision-sync --market um --symbols BTCUSDT --kinds trades,aggTrades,fundingRate   # perps
docker compose run --rm batch normalizer vision      # spot zips → silver
docker compose run --rm batch normalizer vision --market um --symbols BTCUSDT   # perp zips → silver (incl. funding)
docker compose run --rm batch normalizer funding-settled --coins BTC --from 2023-01-01   # hyperliquid settled backfill
```

Reconcile: row counts per day vs the dump line counts; spot-check a month
against Binance's published klines.

## Add an exchange (P2 checklist)

1. Adapter in `rust-backend/data-room/crates/adapters/` (parse + `route` + golden
   fixtures from real captures — commit the fixtures).
2. Collector: add a `[[connections]]` block (new venue = new adapter arm
   in `collector`/`normalizer` dispatch).
3. Instruments: extend `normalizer instruments`.
4. No schema, layout, or Terraform changes — that's the test (spec §12).

## Gap post-mortem

1. `SELECT * FROM gaps WHERE date = '…'` (catalog view) — kind + span.
2. Cross-check collector logs in Loki around `gap_start`.
3. If the gap is an upload failure (files stuck on host): check
   `/var/spool/data-room` — anything old means S3/IAM trouble; the
   30-second sweep retries forever, fix access and it drains itself.
4. Trades on Binance-covered days can be repaired by vision daily dumps
   (`vision-sync` next run); Coinbase quotes cannot — record and move on.

## Query the lake

```bash
DATA_ROOM=s3://$BUCKET duckdb -init rust-backend/data-room/catalog/catalog.sql
# views: trades, book_top, instruments(_latest), bars, rv, gaps
```
