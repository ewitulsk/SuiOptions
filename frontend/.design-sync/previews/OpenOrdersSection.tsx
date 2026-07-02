import { OpenOrdersSection } from "tideline-frontend";

// Open DeepBook orders for a bucket's pool, the "orders" tab of BuyDetailTabs:
// a header with the live count + cancel-all/withdraw controls, then a
// side/options/price table with per-row cancel. It owns the BalanceManager read,
// which is gated on a connected wallet; with none present it renders its real
// pre-trade prompt: "enable trading to see open orders". (The populated order
// table is data-gated on a connected wallet with resting DeepBook orders.)

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

export const EnablePrompt = () => <OpenOrdersSection bucket={bucket} series={series} />;
