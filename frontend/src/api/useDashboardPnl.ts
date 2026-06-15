import { useQuery } from "@tanstack/react-query";
import { fetchDashboardPnl, type BucketPnl } from "./client";

/**
 * True cost-basis PnL for `wallet` (SO-209): per-bucket FIFO remaining lots +
 * realized PnL, from api-service `/dashboard/pnl`. `bm` is the wallet's DeepBook
 * BalanceManager id, so secondary-market buys/sells are attributed; pass `null`
 * for session/custody users (no personal BM → no fills). The dashboard
 * reconciles the returned lots against current holdings and adds unrealized
 * from the live spot.
 */
export function useDashboardPnl(wallet: string | null, bm: string | null) {
  return useQuery<BucketPnl[], Error>({
    queryKey: ["dashboard-pnl", wallet, bm],
    queryFn: () => (wallet ? fetchDashboardPnl(wallet, bm) : Promise.resolve([])),
    enabled: wallet !== null,
    refetchInterval: 5_000,
  });
}
