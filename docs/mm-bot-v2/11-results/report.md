# Backtester study results

Generated 2026-09-02T15:00:44.902128+00:00 from `.`. Every number is a conditional simulation (doc 08 §0.2); see the label roster at the end.

## Doc 07 §5 reproduction (tolerance: turnover within 35% of doc 07, 25% of doc 10 §2)

| band %NAV | turnover ×NAV/30d | doc 07 | doc 10 §2 | vs doc 07 | vs doc 10 | cost %NAV/30d | doc 07 @3.5bp | fees only %NAV/30d | year-end NAV | max DD | liq | margin | ok |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|
| 1.5 | 50.2 | 76.4 | 60.2 | -34.3% | -16.6% | 3.52 | 2.67 | 1.76 | 938214 | 0.484 | 0 | none(doc07_reproduction) | yes |
| 3 | 34.3 | 48.5 | 42.6 | -29.2% | -19.4% | 2.41 | 1.70 | 1.20 | 1104630 | 0.419 | 0 | none(doc07_reproduction) | yes |
| 5 | 24.6 | 33.1 | 31.7 | -25.8% | -22.5% | 1.72 | 1.16 | 0.86 | 1184451 | 0.386 | 0 | none(doc07_reproduction) | yes |
| 10 | 14.9 | 19.1 | 19.1 | -21.8% | -21.8% | 1.05 | 0.67 | 0.52 | 1308386 | 0.380 | 0 | none(doc07_reproduction) | yes |
| 20 | 10.8 | 11.3 | 11.8 | -4.0% | -8.1% | 0.76 | 0.39 | 0.38 | 1586596 | 0.317 | 0 | none(doc07_reproduction) | yes |
| 30 | 6.0 | 8.3 | 7.2 | -27.4% | -16.3% | 0.42 | 0.29 | 0.21 | 1190081 | 0.357 | 0 | none(doc07_reproduction) | yes |

All within tolerance: **true**.

## Walk-forward: btc-estimator (objective `depositor_net_return_annualized`, gate drawdown ≤ 15%)

Folds:

| fold | kind | from | to | data readable from |
|---|---|---|---|---|
| train-1 | Train | 2021-01-01 | 2021-12-31 | 2020-01-01 |
| train-2 | Train | 2022-01-01 | 2022-12-31 | 2020-12-30 |
| train-3 | Train | 2023-01-01 | 2023-12-31 | 2021-12-30 |
| validation-1 | Validation | 2024-01-01 | 2024-12-31 | 2022-12-30 |
| holdout | Holdout | 2025-01-01 | 2026-07-31 | 2023-12-31 |

| candidate | eligible | train mean net (ann.) | train folds | validation net (ann.) | validation max DD | validation liq |
|---|---|---:|---|---|---:|---:|
| windows | no (training drawdown 0.325 > gate 0.15) | +17.0% | +22.3%, +15.9%, +12.9% | +25.4% | 0.302 | 0 |
| har-q0.25 | no (training drawdown 0.353 > gate 0.15) | +97.5% | +205.0%, +18.8%, +68.6% | +85.2% | 0.123 | 0 |
| har-q0.35 | no (training drawdown 0.244 > gate 0.15) | +92.3% | +223.5%, +8.5%, +44.8% | +63.8% | 0.123 | 0 |
| har-q0.45 | no (training drawdown 0.297 > gate 0.15) | +66.0% | +164.2%, -2.0%, +35.8% | +45.5% | 0.134 | 0 |

Selected on training folds only (`ranked_on = ["train"]`): **har-q0.25** (train score +97.5%, every candidate failed the gate). Validation of the selected candidate: mean +85.2% median +85.2% ci95 [n/a, n/a] over 1 fold(s); lower bound clears hurdle 12.0%: **false**.

Holdout: **SEALED** (not opened).

Per-run detail:

| candidate | fold | net (ann.) | max DD | liq | fills | σ paid | σ realized | turnover ×NAV/30d | bankrupt |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| windows | train-1 | +22.3% | 0.325 | 0 | 8 | 0.700 | 0.817 | 6.6 | false |
| windows | train-2 | +15.9% | 0.281 | 0 | 12 | 0.545 | 0.644 | 10.7 | false |
| windows | train-3 | +12.9% | 0.258 | 0 | 13 | 0.366 | 0.424 | 12.0 | false |
| har-q0.25 | train-1 | +205.0% | 0.353 | 0 | 12 | 0.504 | 0.899 | 30.6 | false |
| har-q0.25 | train-2 | +18.8% | 0.197 | 0 | 13 | 0.492 | 0.642 | 13.4 | false |
| har-q0.25 | train-3 | +68.6% | 0.195 | 0 | 13 | 0.259 | 0.424 | 18.7 | false |
| har-q0.35 | train-1 | +223.5% | 0.211 | 0 | 12 | 0.581 | 0.898 | 30.4 | false |
| har-q0.35 | train-2 | +8.5% | 0.244 | 0 | 13 | 0.526 | 0.642 | 12.5 | false |
| har-q0.35 | train-3 | +44.8% | 0.185 | 0 | 13 | 0.294 | 0.424 | 14.9 | false |
| har-q0.45 | train-1 | +164.2% | 0.200 | 0 | 10 | 0.622 | 0.950 | 20.2 | false |
| har-q0.45 | train-2 | -2.0% | 0.297 | 0 | 13 | 0.558 | 0.641 | 11.2 | false |
| har-q0.45 | train-3 | +35.8% | 0.178 | 0 | 13 | 0.320 | 0.424 | 13.7 | false |
| windows | validation-1 | +25.4% | 0.302 | 0 | 13 | 0.444 | 0.530 | 12.8 | false |
| har-q0.25 | validation-1 | +85.2% | 0.123 | 0 | 13 | 0.364 | 0.529 | 20.3 | false |
| har-q0.35 | validation-1 | +63.8% | 0.123 | 0 | 13 | 0.403 | 0.529 | 17.7 | false |
| har-q0.45 | validation-1 | +45.5% | 0.134 | 0 | 13 | 0.436 | 0.528 | 15.7 | false |

## Walk-forward: sui-estimator (objective `depositor_net_return_annualized`, gate drawdown ≤ 15%)

Folds:

| fold | kind | from | to | data readable from |
|---|---|---|---|---|
| train-1 | Train | 2024-05-01 | 2024-10-31 | 2023-05-03 |
| train-2 | Train | 2024-11-01 | 2025-04-30 | 2023-10-31 |
| validation-1 | Validation | 2025-05-01 | 2025-10-31 | 2024-04-29 |
| holdout | Holdout | 2025-11-01 | 2026-07-31 | 2024-10-30 |

| candidate | eligible | train mean net (ann.) | train folds | validation net (ann.) | validation max DD | validation liq |
|---|---|---:|---|---|---:|---:|
| windows | no (training drawdown 0.249 > gate 0.15) | +37.9% | +8.5%, +67.3% | -31.1% | 0.378 | 0 |
| har-q0.25 | no (training drawdown 0.245 > gate 0.15) | +77.8% | +42.7%, +112.8% | -4.1% | 0.215 | 0 |
| har-q0.35 | no (training drawdown 0.252 > gate 0.15) | +64.0% | +16.4%, +111.5% | -19.4% | 0.238 | 0 |
| har-q0.45 | no (training drawdown 0.279 > gate 0.15) | +26.0% | -11.3%, +63.2% | -27.7% | 0.310 | 0 |

Selected on training folds only (`ranked_on = ["train"]`): **har-q0.25** (train score +77.8%, every candidate failed the gate). Validation of the selected candidate: mean -4.1% median -4.1% ci95 [n/a, n/a] over 1 fold(s); lower bound clears hurdle 12.0%: **false**.

Holdout: **SEALED** (not opened).

Per-run detail:

| candidate | fold | net (ann.) | max DD | liq | fills | σ paid | σ realized | turnover ×NAV/30d | bankrupt |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| windows | train-1 | +8.5% | 0.249 | 0 | 6 | 0.779 | 1.025 | 5.7 | false |
| windows | train-2 | +67.3% | 0.249 | 0 | 5 | 1.044 | 1.298 | 5.6 | false |
| har-q0.25 | train-1 | +42.7% | 0.245 | 0 | 7 | 0.827 | 1.096 | 7.4 | false |
| har-q0.25 | train-2 | +112.8% | 0.123 | 0 | 6 | 0.862 | 1.341 | 8.1 | false |
| har-q0.35 | train-1 | +16.4% | 0.252 | 0 | 7 | 1.000 | 1.012 | 7.1 | false |
| har-q0.35 | train-2 | +111.5% | 0.128 | 0 | 6 | 0.962 | 1.335 | 8.2 | false |
| har-q0.45 | train-1 | -11.3% | 0.279 | 0 | 7 | 1.074 | 1.121 | 5.9 | false |
| har-q0.45 | train-2 | +63.2% | 0.119 | 0 | 4 | 1.011 | 1.302 | 4.4 | false |
| windows | validation-1 | -31.1% | 0.378 | 0 | 7 | 0.916 | 1.009 | 5.9 | false |
| har-q0.25 | validation-1 | -4.1% | 0.215 | 0 | 7 | 0.848 | 1.009 | 7.5 | false |
| har-q0.35 | validation-1 | -19.4% | 0.238 | 0 | 7 | 0.868 | 1.036 | 6.8 | false |
| har-q0.45 | validation-1 | -27.7% | 0.310 | 0 | 7 | 0.953 | 1.009 | 6.1 | false |

## Synthetic stress suite `stress` (doc 08 §9.5; limits: 15% historical / 25% stress drawdown, zero liquidations)

| case | limit | NAV end | Δ vs historical | net (ann.) | max DD | liq | closest headroom | bankrupt | exercise cost | pass |
|---|---:|---:|---:|---:|---:|---:|---:|---|---:|---|
| historical | 0.15 | 1062563 | 0 | +10.2% | 0.051 | 1 | -0.030 | false | 2841 | **FAIL** |
| gap_down_60 | 0.25 | 1061957 | -605 | +10.1% | 0.131 | 5 | -25.149 | false | 4320 | **FAIL** |
| gap_up_80 | 0.25 | 1350805 | 288242 | +63.3% | 0.054 | 1 | -2.701 | false | 2967 | **FAIL** |
| crash_multistep_delayed_oracle | 0.25 | 1190234 | 127672 | +32.4% | 0.059 | 1 | -0.293 | false | 3428 | **FAIL** |
| rally_multistep_delayed_oracle | 0.25 | 1049276 | -13287 | +8.0% | 0.095 | 4 | -2.998 | false | 2933 | **FAIL** |
| flat_six_months | 0.25 | 964999 | -97564 | -6.8% | 0.051 | 0 | +0.292 | false | 955 | PASS |
| vol_collapse_after_purchase | 0.25 | 1012669 | -49894 | +2.0% | 0.051 | 0 | +0.292 | false | 1189 | PASS |
| funding_plus_50 | 0.25 | 1055092 | -7471 | +8.9% | 0.062 | 3 | -0.441 | false | 2686 | **FAIL** |
| funding_minus_50 | 0.25 | 1073924 | 11362 | +12.1% | 0.065 | 1 | -0.030 | false | 2849 | **FAIL** |
| venue_outage_exercise_margin | 0.25 | 1128843 | 66280 | +21.5% | 0.074 | 4 | -5.294 | false | 3153 | **FAIL** |
| sui_congestion_near_expiry | 0.25 | 1076928 | 14365 | +12.6% | 0.051 | 1 | -0.030 | false | 2813 | **FAIL** |
| no_resale | 0.25 | 1062563 | 0 | +10.2% | 0.051 | 1 | -0.030 | false | 2841 | **FAIL** |
| no_base_flash | 0.25 | 1062563 | 0 | +10.2% | 0.051 | 1 | -0.030 | false | 2841 | **FAIL** |
| no_quote_flash | 0.25 | 1062563 | 0 | +10.2% | 0.051 | 1 | -0.030 | false | 2841 | **FAIL** |
| router_depth_collapse | 0.25 | 1066161 | 3598 | +10.8% | 0.051 | 0 | +0.032 | false | 10921 | PASS |
| concentrated_expiry | 0.25 | 997498 | -65065 | -0.5% | 0.067 | 2 | -0.562 | false | 5099 | **FAIL** |
| settlement_depeg | 0.25 | 1065288 | 2725 | +10.6% | 0.051 | 2 | -0.099 | false | 2840 | **FAIL** |

Case transformations:

- `historical`: the untouched replay (15% drawdown limit) [transform=none, proxy_oracle, proxy_venue]
- `gap_down_60`: instant −60% gap at the stress instant [transform=price×0.40, proxy_oracle, proxy_venue]
- `gap_up_80`: instant +80% gap at the stress instant [transform=price×1.80, proxy_oracle, proxy_venue]
- `crash_multistep_delayed_oracle`: −12%/day for 5 days with the oracle proxy updating every 5 min at 60 s latency [transform=−12%/day×5, oracle.update_ms=300000, oracle.latency_ms=60000, proxy_oracle, proxy_venue]
- `rally_multistep_delayed_oracle`: +15%/day for 5 days with the oracle proxy updating every 5 min at 60 s latency [transform=+15%/day×5, oracle.update_ms=300000, oracle.latency_ms=60000, proxy_oracle, proxy_venue]
- `flat_six_months`: price pinned (±0.1%) for 183 days from the stress instant; funding zero [transform=flat×183d, funding=0, proxy_oracle, proxy_venue]
- `vol_collapse_after_purchase`: log returns compressed ×0.25 from one day after the stress instant [transform=returns×0.25, proxy_oracle, proxy_venue]
- `funding_plus_50`: +50% annualized funding for 30 days (shorts receive, longs pay) [funding=+0.50/yr×30d, proxy_oracle, proxy_venue]
- `funding_minus_50`: −50% annualized funding for 30 days (shorts pay, longs receive) [funding=−0.50/yr×30d, proxy_oracle, proxy_venue]
- `venue_outage_exercise_margin`: Bluefin outage 12 h before to 36 h after the straddled expiry while the path gaps −25% at the outage start [margin.outages=[expiry−12h, expiry+36h], transform=price×0.75@outage, proxy_oracle, proxy_venue]
- `sui_congestion_near_expiry`: Sui inclusion 10 min ± 5 min, detection 2 min, 20% PTB failure — applied to the whole run (a conservative superset of 'near expiry') [latency.sui_inclusion=600000±300000, latency.indexer_detection=120000, exercise.ptb_failure_prob=0.2, scope=whole_run(conservative), proxy_oracle, proxy_venue]
- `no_resale`: resale disabled (hold to exercise/expiry) [resale.enabled=false, proxy_oracle, proxy_venue]
- `no_base_flash`: DeepBook pool holds no base: put exercise falls through to the quote flash or fails [exercise.pool_base_balance_units=0, proxy_oracle, proxy_venue]
- `no_quote_flash`: DeepBook pool holds no quote: call exercise is cash-only, puts lose the last fallback [exercise.pool_quote_balance=0, proxy_oracle, proxy_venue]
- `router_depth_collapse`: route depth ÷ 20 (each bp of impact absorbs 1/20 of the units) [exercise.route_depth_units_per_bps=÷20, proxy_oracle, proxy_venue]
- `concentrated_expiry`: every writer herds into the nearest listed expiry and the per-expiry cap is lifted to the total budget [flow_gen.herd_prob=1, flow_gen.expiry_concentration=1, limits.per_expiry_max=premium_budget_hard, proxy_oracle, proxy_venue]
- `settlement_depeg`: settlement stablecoin −3% against the perp quote for 7 days, modeled as a −300 bp mark basis (doc 08 §7.4 basis series) [venue.basis=−300bps×7d, depeg=basis_series_proxy, proxy_oracle, proxy_venue]

## Synthetic stress suite `stress-lev3` (doc 08 §9.5; limits: 15% historical / 25% stress drawdown, zero liquidations)

| case | limit | NAV end | Δ vs historical | net (ann.) | max DD | liq | closest headroom | bankrupt | exercise cost | pass |
|---|---:|---:|---:|---:|---:|---:|---:|---|---:|---|
| historical | 0.15 | 1064714 | 0 | +10.5% | 0.051 | 0 | +1.084 | false | 2841 | PASS |
| gap_down_60 | 0.25 | 1172003 | 107289 | +29.1% | 0.064 | 1 | -3.524 | false | 4318 | **FAIL** |
| gap_up_80 | 0.25 | 1324267 | 259553 | +58.0% | 0.051 | 0 | +0.373 | false | 2960 | PASS |
| crash_multistep_delayed_oracle | 0.25 | 1191720 | 127006 | +32.7% | 0.059 | 0 | +3.228 | false | 3428 | PASS |
| rally_multistep_delayed_oracle | 0.25 | 1070466 | 5752 | +11.5% | 0.084 | 1 | -3.471 | false | 2933 | **FAIL** |
| flat_six_months | 0.25 | 965000 | -99714 | -6.8% | 0.051 | 0 | +8.442 | false | 955 | PASS |
| vol_collapse_after_purchase | 0.25 | 1012670 | -52044 | +2.0% | 0.051 | 0 | +8.442 | false | 1189 | PASS |
| funding_plus_50 | 0.25 | 1061022 | -3692 | +9.9% | 0.062 | 0 | +2.832 | false | 2685 | PASS |
| funding_minus_50 | 0.25 | 1076086 | 11372 | +12.4% | 0.065 | 0 | +1.084 | false | 2849 | PASS |
| venue_outage_exercise_margin | 0.25 | 1161803 | 97089 | +27.3% | 0.074 | 1 | -0.048 | false | 3146 | **FAIL** |
| sui_congestion_near_expiry | 0.25 | 1084718 | 20004 | +13.9% | 0.051 | 0 | +2.799 | false | 2830 | PASS |
| no_resale | 0.25 | 1064714 | 0 | +10.5% | 0.051 | 0 | +1.084 | false | 2841 | PASS |
| no_base_flash | 0.25 | 1064714 | 0 | +10.5% | 0.051 | 0 | +1.084 | false | 2841 | PASS |
| no_quote_flash | 0.25 | 1064714 | 0 | +10.5% | 0.051 | 0 | +1.084 | false | 2841 | PASS |
| router_depth_collapse | 0.25 | 1066161 | 1448 | +10.8% | 0.051 | 0 | +1.084 | false | 10921 | PASS |
| concentrated_expiry | 0.25 | 999441 | -65273 | -0.1% | 0.067 | 0 | +3.193 | false | 5099 | PASS |
| settlement_depeg | 0.25 | 1067664 | 2950 | +11.0% | 0.051 | 0 | +1.084 | false | 2841 | PASS |

Case transformations:

- `historical`: the untouched replay (15% drawdown limit) [transform=none, proxy_oracle, proxy_venue]
- `gap_down_60`: instant −60% gap at the stress instant [transform=price×0.40, proxy_oracle, proxy_venue]
- `gap_up_80`: instant +80% gap at the stress instant [transform=price×1.80, proxy_oracle, proxy_venue]
- `crash_multistep_delayed_oracle`: −12%/day for 5 days with the oracle proxy updating every 5 min at 60 s latency [transform=−12%/day×5, oracle.update_ms=300000, oracle.latency_ms=60000, proxy_oracle, proxy_venue]
- `rally_multistep_delayed_oracle`: +15%/day for 5 days with the oracle proxy updating every 5 min at 60 s latency [transform=+15%/day×5, oracle.update_ms=300000, oracle.latency_ms=60000, proxy_oracle, proxy_venue]
- `flat_six_months`: price pinned (±0.1%) for 183 days from the stress instant; funding zero [transform=flat×183d, funding=0, proxy_oracle, proxy_venue]
- `vol_collapse_after_purchase`: log returns compressed ×0.25 from one day after the stress instant [transform=returns×0.25, proxy_oracle, proxy_venue]
- `funding_plus_50`: +50% annualized funding for 30 days (shorts receive, longs pay) [funding=+0.50/yr×30d, proxy_oracle, proxy_venue]
- `funding_minus_50`: −50% annualized funding for 30 days (shorts pay, longs receive) [funding=−0.50/yr×30d, proxy_oracle, proxy_venue]
- `venue_outage_exercise_margin`: Bluefin outage 12 h before to 36 h after the straddled expiry while the path gaps −25% at the outage start [margin.outages=[expiry−12h, expiry+36h], transform=price×0.75@outage, proxy_oracle, proxy_venue]
- `sui_congestion_near_expiry`: Sui inclusion 10 min ± 5 min, detection 2 min, 20% PTB failure — applied to the whole run (a conservative superset of 'near expiry') [latency.sui_inclusion=600000±300000, latency.indexer_detection=120000, exercise.ptb_failure_prob=0.2, scope=whole_run(conservative), proxy_oracle, proxy_venue]
- `no_resale`: resale disabled (hold to exercise/expiry) [resale.enabled=false, proxy_oracle, proxy_venue]
- `no_base_flash`: DeepBook pool holds no base: put exercise falls through to the quote flash or fails [exercise.pool_base_balance_units=0, proxy_oracle, proxy_venue]
- `no_quote_flash`: DeepBook pool holds no quote: call exercise is cash-only, puts lose the last fallback [exercise.pool_quote_balance=0, proxy_oracle, proxy_venue]
- `router_depth_collapse`: route depth ÷ 20 (each bp of impact absorbs 1/20 of the units) [exercise.route_depth_units_per_bps=÷20, proxy_oracle, proxy_venue]
- `concentrated_expiry`: every writer herds into the nearest listed expiry and the per-expiry cap is lifted to the total budget [flow_gen.herd_prob=1, flow_gen.expiry_concentration=1, limits.per_expiry_max=premium_budget_hard, proxy_oracle, proxy_venue]
- `settlement_depeg`: settlement stablecoin −3% against the perp quote for 7 days, modeled as a −300 bp mark basis (doc 08 §7.4 basis series) [venue.basis=−300bps×7d, depeg=basis_series_proxy, proxy_oracle, proxy_venue]

## Capacity frontier (doc 08 §8.6; capacity mode, demand-inelastic injection)

| target accepted/day | mix | feasibility | limit label | min NAV | CI | binding | next | net (ann.) at min NAV | hurdle pass | max DD | liq | accepted RFQs | expiries |
|---:|---|---|---|---:|---|---|---|---:|---:|---:|---:|---:|---:|
| 25000 | balanced | feasible | capital_limited | 743179.55 | [649381.63, 743179.55] | liquidation | premium_per_expiry | 0.15069 | 1.000 | 0.0657 | 0 | 4412.00 | 4066.00 |

## Grid: sui-halfyear (2025-08-01 → 2026-01-31, seeds [1, 2], axes ["hedge.band_pct_nav", "venue.execution_assumption", "margin.leverage", "mix"]) — break-even surface, 0/24 points clear the policy

| point | net median | net mean | ci95 | after idle cost | worst DD | CVaR95 daily | liq | fills | accepted | break-even | binding | limit |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---|---|---|
| hedge.band_pct_nav=10.0 venue.execution_assumption=taker_only margin.leverage=3.0 mix=balanced | +12.4% | +12.4% | [-26.3%, +51.1%] | +8.5% | 0.099 | -0.0045 | 2 | 2944 | 12532699 | no | liquidation | demand_limited |
| hedge.band_pct_nav=10.0 venue.execution_assumption=taker_only margin.leverage=3.0 mix=call_heavy | +5.0% | +5.0% | [-75.7%, +85.6%] | +1.1% | 0.050 | -0.0048 | 1 | 2586 | 10143556 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=taker_only margin.leverage=3.0 mix=put_heavy | +17.4% | +17.4% | [-5.4%, +40.1%] | +13.5% | 0.186 | -0.0052 | 2 | 3057 | 14125471 | no | liquidation | capital_limited |
| hedge.band_pct_nav=10.0 venue.execution_assumption=taker_only margin.leverage=10.0 mix=balanced | +6.3% | +6.3% | [-48.4%, +60.9%] | +2.4% | 0.108 | -0.0046 | 11 | 2945 | 12546172 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=taker_only margin.leverage=10.0 mix=call_heavy | +5.0% | +5.0% | [-44.3%, +54.4%] | +1.1% | 0.050 | -0.0052 | 2 | 2586 | 10143556 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=taker_only margin.leverage=10.0 mix=put_heavy | +20.7% | +20.7% | [+0.8%, +40.7%] | +16.5% | 0.150 | -0.0054 | 10 | 3067 | 14167114 | no | liquidation | demand_limited |
| hedge.band_pct_nav=10.0 venue.execution_assumption=conservative margin.leverage=3.0 mix=balanced | -6.4% | -6.4% | [-18.5%, +5.8%] | -9.7% | 0.185 | -0.0081 | 4 | 2942 | 12548197 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=conservative margin.leverage=3.0 mix=call_heavy | +3.3% | +3.3% | [-71.8%, +78.4%] | -0.5% | 0.049 | -0.0051 | 1 | 2584 | 10136952 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=conservative margin.leverage=3.0 mix=put_heavy | +18.6% | +18.6% | [-64.9%, +102.0%] | +14.7% | 0.242 | -0.0053 | 2 | 3053 | 14109836 | no | liquidation | capital_limited |
| hedge.band_pct_nav=10.0 venue.execution_assumption=conservative margin.leverage=10.0 mix=balanced | -3.6% | -3.6% | [-32.8%, +25.7%] | -7.1% | 0.163 | -0.0058 | 9 | 2942 | 12545991 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=conservative margin.leverage=10.0 mix=call_heavy | +2.9% | +2.9% | [-77.5%, +83.2%] | -1.0% | 0.055 | -0.0051 | 2 | 2584 | 10136952 | no | liquidation | uneconomic |
| hedge.band_pct_nav=10.0 venue.execution_assumption=conservative margin.leverage=10.0 mix=put_heavy | -6.2% | -6.2% | [-58.5%, +46.2%] | -9.6% | 0.242 | -0.0104 | 10 | 3018 | 13976057 | no | liquidation | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=taker_only margin.leverage=3.0 mix=balanced | +14.2% | +14.2% | [-97.2%, +125.6%] | +10.2% | 0.072 | -0.0063 | 1 | 2927 | 12467431 | no | liquidation | demand_limited |
| hedge.band_pct_nav=20.0 venue.execution_assumption=taker_only margin.leverage=3.0 mix=call_heavy | +5.2% | +5.2% | [-31.7%, +42.0%] | +1.3% | 0.064 | -0.0062 | 0 | 2573 | 10105167 | no | median net 0.0518 < hurdle 0.1200 | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=taker_only margin.leverage=3.0 mix=put_heavy | +16.0% | +16.0% | [-131.9%, +163.8%] | +12.2% | 0.196 | -0.0065 | 3 | 3045 | 14071216 | no | liquidation | demand_limited |
| hedge.band_pct_nav=20.0 venue.execution_assumption=taker_only margin.leverage=10.0 mix=balanced | +19.6% | +19.6% | [+10.9%, +28.3%] | +15.3% | 0.061 | -0.0061 | 5 | 2926 | 12466913 | no | liquidation | demand_limited |
| hedge.band_pct_nav=20.0 venue.execution_assumption=taker_only margin.leverage=10.0 mix=call_heavy | +5.2% | +5.2% | [-31.2%, +41.6%] | +1.3% | 0.064 | -0.0062 | 1 | 2573 | 10105167 | no | liquidation | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=taker_only margin.leverage=10.0 mix=put_heavy | +13.3% | +13.3% | [-33.5%, +60.0%] | +9.3% | 0.163 | -0.0062 | 11 | 3055 | 14112449 | no | liquidation | demand_limited |
| hedge.band_pct_nav=20.0 venue.execution_assumption=conservative margin.leverage=3.0 mix=balanced | +8.2% | +8.2% | [-37.4%, +53.7%] | +4.3% | 0.085 | -0.0073 | 1 | 2925 | 12460845 | no | liquidation | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=conservative margin.leverage=3.0 mix=call_heavy | +4.8% | +4.8% | [-29.7%, +39.4%] | +1.0% | 0.048 | -0.0063 | 0 | 2571 | 10089227 | no | median net 0.0483 < hurdle 0.1200 | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=conservative margin.leverage=3.0 mix=put_heavy | +7.7% | +7.7% | [-171.0%, +186.4%] | +4.2% | 0.190 | -0.0094 | 3 | 3016 | 13977734 | no | liquidation | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=conservative margin.leverage=10.0 mix=balanced | +12.0% | +12.0% | [-36.1%, +60.1%] | +8.0% | 0.064 | -0.0064 | 4 | 2926 | 12463403 | no | liquidation | demand_limited |
| hedge.band_pct_nav=20.0 venue.execution_assumption=conservative margin.leverage=10.0 mix=call_heavy | +4.8% | +4.8% | [-29.7%, +39.4%] | +0.9% | 0.048 | -0.0063 | 0 | 2571 | 10089227 | no | median net 0.0483 < hurdle 0.1200 | uneconomic |
| hedge.band_pct_nav=20.0 venue.execution_assumption=conservative margin.leverage=10.0 mix=put_heavy | -8.2% | -8.2% | [-19.5%, +3.1%] | -11.5% | 0.212 | -0.0125 | 8 | 3002 | 13921567 | no | liquidation | uneconomic |

Sensitivity (other axes at their base value):

| axis | values | median net | break-even | range |
|---|---|---|---|---:|
| hedge.band_pct_nav | 10.0, 20.0 | +12.4%, +14.2% | [false, false] | +1.8% |
| venue.execution_assumption | taker_only, conservative | +12.4%, -6.4% | [false, false] | +18.8% |
| margin.leverage | 3.0, 10.0 | +12.4%, +6.3% | [false, false] | +6.1% |
| mix | balanced, call_heavy, put_heavy | +12.4%, +5.0%, +17.4% | [false, false, false] | +12.4% |

## Doc 08 §12 — definition of validated

| # | item | status | why |
|---:|---|---|---|
| 1 | Exact ledger reconciliation passes every event and full replay | **pass** | worst /Σlines − ΔNAV//NAV0 over 110 runs = 2.65e-11; the option and perp identities close by construction (attribution.json) |
| 2 | Live and simulation adapters produce identical commands for identical event traces | **not_testable_here** | PR I kernel smoke (`kernel::tests`) drives the shared DeskKernel from the backtester; a recorded live trace replayed through both adapters does not exist yet |
| 3 | The strategy cannot create written options | **by_construction** | the engine has no write path: positions enter the ledger only through an accepted RFQ the desk BUYS (engine::on_flow) |
| 4 | Calls and puts both quote, reserve, hedge, resell, expire, and exercise correctly | **pass** | engine::tests (generated_flow_with_hazard_acceptance_reserves_then_fills_or_expires, call_sweep_exercises_itm_before_expiry_and_failed_ptbs_move_nothing, put_sweep_routes_like_the_live_waterfall, solver::tests::market_mode_labels_and_no_resale_run_completes) |
| 5 | All three put PTBs and their fallback order pass atomic failure tests | **pass** | exercise::tests::put_route_goldens_match_the_shared_fixture + engine::tests::put_sweep_routes_like_the_live_waterfall (vault_underlying → base_flash → quote_flash → capacity reject; failed PTB moves nothing) |
| 6 | No-resale mode completes and is economically survivable | **pass** | stress: nav_end 1062563, drawdown 0.051 (limit 0.25), liquidations 1; stress-lev3: nav_end 1064714, drawdown 0.051 (limit 0.25), liquidations 0 |
| 7 | Results clear the predeclared return hurdle on the untouched holdout | **sealed** | 2 walk-forward manifest(s), holdout not opened (`--open-holdout` absent) |
| 8 | The lower confidence bound, not only the mean, clears the chosen hurdle | **fail** | btc-estimator validation(1 folds): mean +0.8523 ci95 [n/a, n/a] lower-clears=false; sui-estimator validation(1 folds): mean -0.0408 ci95 [n/a, n/a] lower-clears=false |
| 9 | Agreed historical and synthetic stresses remain inside drawdown and liquidation limits | **fail** | stress: 14/17 cases outside limits: historical (dd 0.051/0.15, liq 1), gap_down_60 (dd 0.131/0.25, liq 5), gap_up_80 (dd 0.054/0.25, liq 1), crash_multistep_delayed_oracle (dd 0.059/0.25, liq 1), rally_multistep_delayed_oracle (dd 0.095/0.25, liq 4), funding_plus_50 (dd 0.062/0.25, liq 3), funding_minus_50 (dd 0.065/0.25, liq 1), venue_outage_exercise_margin (dd 0.074/0.25, liq 4), sui_congestion_near_expiry (dd 0.051/0.25, liq 1), no_resale (dd 0.051/0.25, liq 1), no_base_flash (dd 0.051/0.25, liq 1), no_quote_flash (dd 0.051/0.25, liq 1), concentrated_expiry (dd 0.067/0.25, liq 2), settlement_depeg (dd 0.051/0.25, liq 2) / stress-lev3: 3/17 cases outside limits: gap_down_60 (dd 0.064/0.25, liq 1), rally_multistep_delayed_oracle (dd 0.084/0.25, liq 1), venue_outage_exercise_margin (dd 0.074/0.25, liq 1) |
| 10 | Margin top-ups remain feasible without violating premium/liquidity constraints | **pass** | stress: top-ups 18 (declined 0, rejected 0), liquidations 1, closest headroom Some(-0.0298783456104737); stress-lev3: top-ups 0 (declined 0, rejected 0), liquidations 0, closest headroom Some(1.0836019811445448) |
| 11 | Results remain acceptable across call-heavy, put-heavy, and mixed flow | **fail** | 0/24 mix points break even: mix=balanced hedge.band_pct_nav=10.0/venue.execution_assumption=taker_only/margin.leverage=3.0=FAIL; mix=call_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=taker_only/margin.leverage=3.0=FAIL; mix=put_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=taker_only/margin.leverage=3.0=FAIL; mix=balanced hedge.band_pct_nav=10.0/venue.execution_assumption=taker_only/margin.leverage=10.0=FAIL; mix=call_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=taker_only/margin.leverage=10.0=FAIL; mix=put_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=taker_only/margin.leverage=10.0=FAIL; mix=balanced hedge.band_pct_nav=10.0/venue.execution_assumption=conservative/margin.leverage=3.0=FAIL; mix=call_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=conservative/margin.leverage=3.0=FAIL; mix=put_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=conservative/margin.leverage=3.0=FAIL; mix=balanced hedge.band_pct_nav=10.0/venue.execution_assumption=conservative/margin.leverage=10.0=FAIL; mix=call_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=conservative/margin.leverage=10.0=FAIL; mix=put_heavy hedge.band_pct_nav=10.0/venue.execution_assumption=conservative/margin.leverage=10.0=FAIL; mix=balanced hedge.band_pct_nav=20.0/venue.execution_assumption=taker_only/margin.leverage=3.0=FAIL; mix=call_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=taker_only/margin.leverage=3.0=FAIL; mix=put_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=taker_only/margin.leverage=3.0=FAIL; mix=balanced hedge.band_pct_nav=20.0/venue.execution_assumption=taker_only/margin.leverage=10.0=FAIL; mix=call_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=taker_only/margin.leverage=10.0=FAIL; mix=put_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=taker_only/margin.leverage=10.0=FAIL; mix=balanced hedge.band_pct_nav=20.0/venue.execution_assumption=conservative/margin.leverage=3.0=FAIL; mix=call_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=conservative/margin.leverage=3.0=FAIL; mix=put_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=conservative/margin.leverage=3.0=FAIL; mix=balanced hedge.band_pct_nav=20.0/venue.execution_assumption=conservative/margin.leverage=10.0=FAIL; mix=call_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=conservative/margin.leverage=10.0=FAIL; mix=put_heavy hedge.band_pct_nav=20.0/venue.execution_assumption=conservative/margin.leverage=10.0=FAIL |
| 12 | Profit does not depend on one latency, queue, IV, resale, or flow-seed assumption | **fail** | hedge.band_pct_nav: medians ["+0.124", "+0.142"] break-even [false, false]; venue.execution_assumption: medians ["+0.124", "-0.064"] break-even [false, false]; margin.leverage: medians ["+0.124", "+0.063"] break-even [false, false]; mix: medians ["+0.124", "+0.050", "+0.174"] break-even [false, false, false] |
| 13 | Capacity is bounded by measured hedge depth, flash balances, router depth, and expiry concentration | **fail** | every capacity result is labeled venue_capacity=assumed / flash_capacity=assumed: no pool-balance poller and no Bluefin depth history exist (doc 08 §10) |
| 14 | Every target Earn volume has a minimum-NAV estimate, confidence interval, binding constraint, and feasibility label | **pass** | V=25000 balanced: feasible min_nav=743179.55 ci=[649381.63,743179.55] binding=liquidation label=capital_limited |
| 15 | Model edge is never presented as realized revenue | **by_construction** | the only edge line is `model_edge_at_entry` (attribution.json: note_model_edge; Metric::model_edge_at_entry) and it is excluded from every return figure, which is the CAGR of exact NAV |
| 16 | Every published result includes uncertainty, data coverage, and proxy labels | **pass** | 110 runs carry 33 distinct labels; coverage and invalidated spans on every Metric; distributions carry n, sd, quantiles, t-interval, CVaR |

## Label roster (every assumption carried by at least one published result)

- `acceptance=hazard_ttl`
- `acceptance=instant`
- `band_pct_nav=1.5`
- `band_pct_nav=10`
- `band_pct_nav=20`
- `band_pct_nav=3`
- `band_pct_nav=30`
- `band_pct_nav=5`
- `basis_configured=false`
- `basis_configured=true`
- `estimator=har(q_bid=0.25)`
- `estimator=har(q_bid=0.35)`
- `estimator=har(q_bid=0.45)`
- `estimator=windows`
- `execution=taker_only`
- `exercise=american_sweep`
- `flash_capacity_assumed=true`
- `flow=capacity_injection(demand_inelastic)`
- `flow=constant`
- `flow=generated_market`
- `flow_provenance=prior (stated, uncalibrated: doc 08 §3.1 2026-09-01)`
- `gap_policy=invalidate`
- `latency_assumed=true`
- `margin_model=isolated(bluefin_rules)`
- `margin_model=none(doc07_reproduction)`
- `max_lean=0.8`
- `proxy_oracle=true`
- `proxy_venue=true`
- `resale=no_resale`
- `risk_premium=0.05`
- `sui_inclusion_ms=1500`
- `sui_inclusion_ms=600000`
- `venue_capacity=assumed`
