import { useQuery } from "@tanstack/react-query";
import { fetchPositions, type Position } from "./client";

/**
 * `Position` objects (writer-side covered-call rows) owned by `wallet`.
 * Returns an empty array — not a disabled query — when `wallet` is null
 * so consumers don't need a separate "no wallet" branch.
 */
export function usePositions(wallet: string | null) {
  return useQuery<Position[], Error>({
    queryKey: ["positions", wallet],
    queryFn: () => (wallet ? fetchPositions(wallet) : Promise.resolve([])),
    enabled: wallet !== null,
    refetchInterval: 5_000,
  });
}
