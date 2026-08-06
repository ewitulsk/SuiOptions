// Read-only Bluefin Pro data via the hedge-signer's allowlisted
// /bluefin/:host relay (Bluefin serves CORS only to its own origins).
// The parent account address comes from the hedge-signer's per-vault
// FROST group key.

import { useQuery } from "@tanstack/react-query";

import { useServiceUrls } from "../config";

async function relay(hedgeSigner: string, host: "data" | "auth" | "trade", path: string) {
  const res = await fetch(`${hedgeSigner}/bluefin/${host}${path}`);
  return res;
}

export async function fetchFrostParent(
  hedgeSigner: string,
  vaultId: string,
): Promise<{ groupPublicKeyHex: string; suiAddress: string } | null> {
  const res = await fetch(`${hedgeSigner}/frost/pubkey/${encodeURIComponent(vaultId)}`);
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`hedge-signer /frost/pubkey: ${res.status}`);
  const body = (await res.json()) as { group_public_key_hex: string; sui_address: string };
  return { groupPublicKeyHex: body.group_public_key_hex, suiAddress: body.sui_address };
}

export function useFrostParent(vaultId: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["frostParent", urls.hedgeSigner, vaultId],
    queryFn: () => fetchFrostParent(urls.hedgeSigner, vaultId as string),
    enabled: Boolean(vaultId),
    staleTime: 5 * 60_000,
    retry: 1,
  });
}

export type BluefinPosition = {
  symbol: string;
  side: string;
  sizeE9: string;
  avgEntryPriceE9: string;
  markPriceE9: string;
  liquidationPriceE9: string;
  unrealizedPnlE9: string;
  leverageE9?: string;
  isIsolated: boolean;
  fundingRatePaymentSinceOpenedE9?: string;
};

export type BluefinAccount = {
  totalAccountValueE9: string;
  marginAvailableE9?: string;
  crossMarginRequiredE9?: string;
  totalUnrealizedPnlE9?: string;
  crossLeverageE9?: string;
  canTrade?: boolean;
  assets?: Array<{ symbol: string; quantityE9: string }>;
  positions?: BluefinPosition[];
  authorizedWallets?: Array<string | { address?: string; wallet?: string }>;
  updatedAtMillis?: number;
};

/** Public, no-auth account read; null for an unknown account (accounts
 * materialize on first deposit). */
export async function fetchBluefinAccount(
  hedgeSigner: string,
  accountAddress: string,
): Promise<BluefinAccount | null> {
  const res = await relay(
    hedgeSigner,
    "data",
    `/api/v1/account?accountAddress=${encodeURIComponent(accountAddress)}`,
  );
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`bluefin account read failed: ${res.status}`);
  const body = (await res.json()) as BluefinAccount & { message?: string };
  if (body.totalAccountValueE9 == null) return null;
  return body;
}

export function useBluefinAccount(accountAddress: string | null | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["bluefinAccount", urls.hedgeSigner, accountAddress],
    queryFn: () => fetchBluefinAccount(urls.hedgeSigner, accountAddress as string),
    enabled: Boolean(accountAddress),
    refetchInterval: 30_000,
    retry: 1,
  });
}

export type FundingPoint = {
  fundingRateE9: string;
  fundingTimeAtMillis: number;
  symbol: string;
};

/** `GET /v1/exchange/fundingRateHistory` (public; hourly points). */
export async function fetchFundingHistory(
  hedgeSigner: string,
  symbol: string,
  limit = 168,
): Promise<FundingPoint[]> {
  const res = await relay(
    hedgeSigner,
    "data",
    `/v1/exchange/fundingRateHistory?symbol=${encodeURIComponent(symbol)}&limit=${limit}`,
  );
  if (!res.ok) throw new Error(`funding history failed: ${res.status}`);
  return (await res.json()) as FundingPoint[];
}

export function useFundingHistory(symbol: string | undefined) {
  const urls = useServiceUrls();
  return useQuery({
    queryKey: ["bluefinFunding", urls.hedgeSigner, symbol],
    queryFn: () => fetchFundingHistory(urls.hedgeSigner, symbol as string),
    enabled: Boolean(symbol),
    refetchInterval: 5 * 60_000,
    retry: 1,
  });
}

export const E9 = 1e9;

export function fromE9(v: string | null | undefined): number | null {
  if (v == null) return null;
  const n = Number(v);
  return Number.isFinite(n) ? n / E9 : null;
}

/** Hourly funding rate → annualized (rate × 24 × 365). */
export function annualizedFunding(hourlyRateE9: string): number {
  return (Number(hourlyRateE9) / E9) * 24 * 365;
}

/** Distance to liquidation as a fraction of mark (null when flat). */
export function liqDistance(p: BluefinPosition): number | null {
  const mark = fromE9(p.markPriceE9);
  const liq = fromE9(p.liquidationPriceE9);
  if (mark == null || liq == null || mark <= 0 || liq <= 0) return null;
  return Math.abs(mark - liq) / mark;
}
