// Single source of truth for which on-chain deployment the app targets.
//
// The package / protocol-config / treasury ids and the supported-token
// catalog are fetched at startup from the **token-info service** (the only
// service that reads deployments.json). `initConfig()` must be awaited before
// the app renders — see `main.tsx`. This is a hard cutover: if token-info is
// unreachable the app fails to boot (there is no deployments.json fallback).
//
// Ids are `string | undefined`: an environment with no published deployment
// yields `undefined`, and the screens fall back to their existing "no
// deployment configured" / empty states instead of crashing.

export type SuiEnvironment = "mainnet" | "testnet" | "devnet";

// Default to testnet so local dev runs with no env file. Selects the Sui
// network for dapp-kit; the on-chain ids come from token-info.
export const ENV: SuiEnvironment =
  (import.meta.env.VITE_ENVIRONMENT as SuiEnvironment | undefined) ?? "testnet";

// token-info public base URL. Local dev hits the service directly; deployed
// builds set VITE_TOKEN_INFO_URL to the env's public route
// (e.g. https://<host>/<env>/token-info).
export const TOKEN_INFO_URL: string =
  (import.meta.env.VITE_TOKEN_INFO_URL as string | undefined) ??
  "http://127.0.0.1:9005";

// auth-service public base URL. Local dev hits the service directly; deployed
// builds set VITE_AUTH_URL to the env's public route
// (e.g. https://<host>/<env>/auth). Used by the admin token-manager to obtain
// the JWT that gates token-info's mutate endpoints.
export const AUTH_URL: string =
  (import.meta.env.VITE_AUTH_URL as string | undefined) ?? "http://127.0.0.1:9007";

// Populated by `initConfig()`. Exported as live bindings — consumers that
// `import { PACKAGE_ID }` see the value once initialization completes (which
// happens before the first render).
export let PACKAGE_ID: string | undefined;
export let PROTOCOL_CONFIG_ID: string | undefined;
export let TREASURY_ID: string | undefined;

// Testnet faucet tokens (SO-93). Each is a shared `Faucet` with a public
// `mint_to_sender`. Only the testnet/dev deployment publishes these; on
// mainnet this stays `[]` and the faucet page shows a "testnet only" state.
export type TestToken = {
  symbol: string;
  /** Full Move coin type, e.g. `0x…::tbtc::TBTC`. */
  coinType: string;
  /** Shared `Faucet` object id. */
  faucetId: string;
  decimals: number;
  /** Module name from the coin type's middle segment, e.g. `tbtc`. */
  module: string;
  /** Package that published the test tokens (≠ the protocol `PACKAGE_ID`). */
  packageId: string;
};

export let TEST_TOKENS: TestToken[] = [];

/** One supported-token catalog entry from token-info's `GET /tokens`. */
export type SupportedToken = {
  coinType: string;
  ticker: string;
  name: string;
  logoUri: string | null;
  decimals: number;
  pythFeedId: string | null;
  enabled: boolean;
};

export let SUPPORTED_TOKENS: SupportedToken[] = [];

// --- wire shapes returned by token-info -------------------------------------

type PackageInfoDto = {
  packageId: string;
  protocolConfigId: string;
  treasuryId?: string | null;
  network?: string;
  testTokens?: {
    packageId: string;
    tokens: Record<string, { coinType: string; faucetId: string; decimals: number }>;
  } | null;
};

type SupportedTokenDto = {
  coin_type: string;
  ticker: string;
  name: string;
  logo_uri?: string | null;
  decimals: number;
  pyth_feed_id?: string | null;
  enabled: boolean;
};

/**
 * Fetch `package_info` + the supported-token catalog from token-info and
 * populate the exported bindings. Throws if token-info is unreachable or
 * returns non-2xx — the caller (main.tsx) surfaces a boot error.
 */
export async function initConfig(): Promise<void> {
  const base = TOKEN_INFO_URL.replace(/\/$/, "");
  const [piRes, tokRes] = await Promise.all([
    fetch(`${base}/package-info`),
    fetch(`${base}/tokens`),
  ]);
  if (!piRes.ok) throw new Error(`token-info /package-info → ${piRes.status}`);
  if (!tokRes.ok) throw new Error(`token-info /tokens → ${tokRes.status}`);

  const info = (await piRes.json()) as PackageInfoDto;
  const tokens = (await tokRes.json()) as SupportedTokenDto[];

  PACKAGE_ID = info.packageId;
  PROTOCOL_CONFIG_ID = info.protocolConfigId;
  TREASURY_ID = info.treasuryId ?? undefined;

  const tt = info.testTokens;
  TEST_TOKENS = tt
    ? Object.entries(tt.tokens).map(([symbol, t]) => ({
        symbol,
        coinType: t.coinType,
        faucetId: t.faucetId,
        decimals: t.decimals,
        module: t.coinType.split("::")[1] ?? symbol.toLowerCase(),
        packageId: tt.packageId,
      }))
    : [];

  SUPPORTED_TOKENS = tokens.map((t) => ({
    coinType: t.coin_type,
    ticker: t.ticker,
    name: t.name,
    logoUri: t.logo_uri ?? null,
    decimals: t.decimals,
    pythFeedId: t.pyth_feed_id ?? null,
    enabled: t.enabled,
  }));
}
