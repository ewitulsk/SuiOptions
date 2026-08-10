// All config is env vars with local-dev defaults — unlike frontend/ there is
// no token-info boot fetch: the exchange block isn't served by token-info, so
// the package id arrives via VITE_EXCHANGE_PACKAGE_ID (see .env.example).

export type SuiEnv = "testnet" | "mainnet" | "devnet";

export const ENV = (import.meta.env.VITE_ENVIRONMENT ?? "testnet") as SuiEnv;

export const ORDERBOOK_URL: string =
  import.meta.env.VITE_ORDERBOOK_URL ?? "http://127.0.0.1:9014";

/** contracts/exchange package id; undefined = writes disabled, screens show a hint. */
export const EXCHANGE_PACKAGE_ID: string | undefined =
  import.meta.env.VITE_EXCHANGE_PACKAGE_ID || undefined;

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
