import { TradePanel } from "tideline-frontend";

// DeepBook trade ticket for a bucket's pool. The BalanceManager lookup keys off
// a connected wallet/session; with none present the query is disabled and the
// panel renders its real first-run setup card: "trade on deepbook" heading, the
// one-time BalanceManager explainer, and a "Connect to trade" CTA. (With a
// wallet it becomes the buy/sell order form + holdings — data-gated in preview.)

const series = {
  asset_symbol: "TBTC",
  asset_decimals: 8,
  asset_coin_type: "0xtbtc::tbtc::TBTC",
  settlement_symbol: "USDC",
  settlement_decimals: 6,
  settlement_coin_type: "0xusdc::usdc::USDC",
  expiry_ms: Date.parse("2026-06-26T08:00:00Z"),
  expiry_iso: "2026-06-26T08:00:00Z",
  buckets: [],
};

const bucket = {
  bucket_id: "0xbucket",
  strike: 96000,
  strike_raw: "96000000000",
  call_coin_type: "0xcall::call::CALL",
  strike_scale: 6,
  total_written: 12.5,
  total_written_raw: "1250000000",
  exercise_cursor: 4.2,
  exercise_cursor_raw: "420000000",
  fill_pct: 33,
  invalidated: false,
  deepbook_pool_id: "0xpool",
  tradeable: true,
};

export const SetupState = () => <TradePanel bucket={bucket} series={series} />;
