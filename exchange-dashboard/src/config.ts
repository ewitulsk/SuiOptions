// All config is env vars with local-dev defaults, one var per backend route
// (same posture as frontend/src/config.ts). On-chain ids are NOT env config:
// the exchange package id rotates on every contract redeploy, so it is
// fetched at runtime from token-info — see hooks/useExchangeInfo.ts.

export type SuiEnv = "testnet" | "mainnet" | "devnet";

export const ENV = (import.meta.env.VITE_ENVIRONMENT ?? "testnet") as SuiEnv;

export const ORDERBOOK_URL: string =
  import.meta.env.VITE_ORDERBOOK_URL ?? "http://127.0.0.1:9014";

// token-info public base URL. Local dev hits the service directly; deployed
// builds set VITE_TOKEN_INFO_URL to the env's public route
// (e.g. https://<host>/<env>/token-info).
export const TOKEN_INFO_URL: string =
  import.meta.env.VITE_TOKEN_INFO_URL ?? "http://127.0.0.1:9005";

const DEFAULT_EXPLORERS: Record<SuiEnv, string> = {
  testnet: "https://testnet.suivision.xyz",
  mainnet: "https://suivision.xyz",
  devnet: "https://devnet.suivision.xyz",
};

export const EXPLORER_URL: string =
  import.meta.env.VITE_EXPLORER_URL || DEFAULT_EXPLORERS[ENV];

export function explorerTxUrl(digest: string): string {
  return `${EXPLORER_URL}/txblock/${digest}`;
}

export function explorerObjectUrl(id: string): string {
  return `${EXPLORER_URL}/object/${id}`;
}
