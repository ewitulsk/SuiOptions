import { useState } from "react";
import { BuyDetailTabs } from "tideline-frontend";
import type { DetailTab } from "tideline-frontend";

// The Greeks · Details · Book · Orders card under the strike chain. Greeks and
// the marks come from /options/metrics (absent in preview → real WaveLoader /
// "—" placeholders). The segmented pill slides between the four tabs.

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

const bucket = {
  bucket_id: "0xbucket96k",
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
  deepbook_pool_id: "0xpool96k",
  tradeable: true,
};

export const Greeks = () => {
  const [tab, setTab] = useState<DetailTab>("greeks");
  return (
    <BuyDetailTabs
      bucket={bucket}
      series={series}
      spot={96850}
      mid={3680}
      wallet={null}
      tab={tab}
      onTabChange={setTab}
    />
  );
};

export const Details = () => {
  const [tab, setTab] = useState<DetailTab>("details");
  return (
    <BuyDetailTabs
      bucket={bucket}
      series={series}
      spot={96850}
      mid={3680}
      wallet={null}
      tab={tab}
      onTabChange={setTab}
    />
  );
};

export const Orders = () => {
  const [tab, setTab] = useState<DetailTab>("orders");
  return (
    <BuyDetailTabs
      bucket={bucket}
      series={series}
      spot={96850}
      mid={3680}
      wallet={null}
      tab={tab}
      onTabChange={setTab}
    />
  );
};
