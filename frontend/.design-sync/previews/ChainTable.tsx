import { ChainTable } from "tideline-frontend";

// Dense Deribit/Aevo-style options chain: one row per strike showing
// Bid · Mark · Ask plus model IV · Δ, with a spot-price divider threaded in.
// Per-row live book/greeks hooks are absent in preview, so bid/ask/iv/Δ render
// their real "—" placeholders while Mark falls back to the indicative premium.

const expiryMs = Date.parse("2026-06-26T08:00:00Z");

const series = {
  asset_symbol: "TBTC",
  asset_decimals: 8,
  asset_coin_type: "0xtbtc::tbtc::TBTC",
  settlement_symbol: "USDC",
  settlement_decimals: 6,
  settlement_coin_type: "0xusdc::usdc::USDC",
  expiry_ms: expiryMs,
  expiry_iso: "2026-06-26T08:00:00Z",
  buckets: [],
};

const STRIKES = [88000, 92000, 96000, 100000, 104000, 108000];
const MARKS = [9120, 6240, 3680, 1990, 940, 410];

const buckets = STRIKES.map((strike, i) => ({
  bucket_id: `0xbucket${i}`,
  strike,
  strike_raw: String(strike * 1e6),
  call_coin_type: `0xcall::call::CALL${i}`,
  strike_scale: 6,
  total_written: 12.5,
  total_written_raw: "1250000000",
  exercise_cursor: 4.2,
  exercise_cursor_raw: "420000000",
  fill_pct: 33,
  invalidated: false,
  deepbook_pool_id: `0xpool${i}`,
  tradeable: true,
}));

const strikes = STRIKES.map((strike, i) => ({
  strike,
  perUnit: MARKS[i] / strike,
  premium: MARKS[i],
  premiumDisplay: MARKS[i].toFixed(2),
}));

export const FullChain = () => (
  <ChainTable
    buckets={buckets}
    strikes={strikes}
    series={series}
    spot={96850}
    selectedIdx={2}
    onSelect={() => {}}
  />
);

export const DeepInTheMoney = () => (
  <ChainTable
    buckets={buckets}
    strikes={strikes}
    series={series}
    spot={110000}
    selectedIdx={0}
    onSelect={() => {}}
  />
);
