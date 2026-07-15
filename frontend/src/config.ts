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

// gas-station public base URL. Sponsors user transactions (pays their gas).
// Local dev hits the service directly; deployed builds set VITE_GAS_STATION_URL
// to the env's public route (e.g. https://<host>/<env>/gas-station).
export const GAS_STATION_URL: string =
  (import.meta.env.VITE_GAS_STATION_URL as string | undefined) ?? "http://127.0.0.1:9009";

// price-charting public base URL (SO-157). Serves OHLC bars (REST) and live
// bar updates (WS at `<base>/ws`, http(s) scheme auto-swapped to ws(s)).
// Deployed builds set VITE_CHARTS_URL to the env's public route
// (e.g. https://<host>/<env>/charts).
export const CHARTS_URL: string =
  (import.meta.env.VITE_CHARTS_URL as string | undefined) ?? "http://127.0.0.1:9011";

// cctp-relay public base URL. Tracks CCTP bridge transfers and auto-relays
// the destination-chain mint. Deployed builds set VITE_CCTP_URL to the env's
// public route (e.g. https://<host>/<env>/cctp).
export const CCTP_URL: string =
  (import.meta.env.VITE_CCTP_URL as string | undefined) ?? "http://127.0.0.1:9015";

// Circle CCTP v1 constants per network (developers.circle.com/cctp/v1).
// These are Circle's own deployments — static per network, so they live here
// rather than in token-info. Our cctp_bridge package ids come from
// token-info (CCTP_BRIDGE_PACKAGE_ID below).
export const CCTP =
  ENV === "mainnet"
    ? {
        domainSui: 8,
        domainSolana: 5,
        suiTokenMessengerPackage:
          "0x2aa6c5d56376c371f88a6cc42e852824994993cb9bab8d3e6450cbe3cb32b94e",
        suiMessageTransmitterState:
          "0xf68268c3d9b1df3215f2439400c1c4ea08ac4ef4bb7d6f3ca6a2a239e17510af",
        suiTokenMessengerState:
          "0x45993eecc0382f37419864992c12faee2238f5cfe22b98ad3bf455baf65c8a2f",
        suiUsdcTreasury:
          "0x57d6725e7a8b49a7b2a612f6bd66ab5f39fc95332ca48be421c3229d514a6de7",
        suiUsdcCoinType:
          "0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC",
        solanaUsdcMint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        solanaBridgeProgram: "77R21RcDcQuhWPkTNHh7BeUgBstF2Nmsysp86QpZam86",
        solanaTokenMessengerProgram: "CCTPiPYPc6AsJuwueEnWgSgucamXDZwBd53dQ11YiKX3",
        solanaMessageTransmitterProgram: "CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd",
        solanaRpcUrl: "https://api.mainnet-beta.solana.com",
      }
    : {
        domainSui: 8,
        domainSolana: 5,
        suiTokenMessengerPackage:
          "0x31cc14d80c175ae39777c0238f20594c6d4869cfab199f40b69f3319956b8beb",
        suiMessageTransmitterState:
          "0x98234bd0fa9ac12cc0a20a144a22e36d6a32f7e0a97baaeaf9c76cdc6d122d2e",
        suiTokenMessengerState:
          "0x5252abd1137094ed1db3e0d75bc36abcd287aee4bc310f8e047727ef5682e7c2",
        suiUsdcTreasury:
          "0x7170137d4a6431bf83351ac025baf462909bffe2877d87716374fb42b9629ebe",
        suiUsdcCoinType:
          "0xa1ec7fc00a6f40db9693ad1415d0c193ad3906494428cf252621037bd7117e29::usdc::USDC",
        solanaUsdcMint: "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
        solanaBridgeProgram: "77R21RcDcQuhWPkTNHh7BeUgBstF2Nmsysp86QpZam86",
        solanaTokenMessengerProgram: "CCTPiPYPc6AsJuwueEnWgSgucamXDZwBd53dQ11YiKX3",
        solanaMessageTransmitterProgram: "CCTPmbSD7gX1bxKPAmg77w8oFzNFpaQiQUWD43TKaecd",
        solanaRpcUrl: "https://api.devnet.solana.com",
      };

// WalletConnect projectId (from dashboard.reown.com). Required to offer the
// "Sign in with WalletConnect" session login; when unset the option simply
// doesn't render (same gating pattern as the session deployment ids).
export const WALLETCONNECT_PROJECT_ID: string | undefined = import.meta.env
  .VITE_WALLETCONNECT_PROJECT_ID as string | undefined;

// Populated by `initConfig()`. Exported as live bindings — consumers that
// `import { PACKAGE_ID }` see the value once initialization completes (which
// happens before the first render).
export let PACKAGE_ID: string | undefined;
export let PROTOCOL_CONFIG_ID: string | undefined;
export let TREASURY_ID: string | undefined;

// DeepBook v3 deployment ids (SO-151), served by token-info alongside the
// protocol ids. All `undefined` on networks without a DeepBook deployment
// (devnet) — DeepBook features simply don't render there.
export let DEEPBOOK_PACKAGE_ID: string | undefined;
/** Original publish id — event/struct TYPES resolve here (BalanceManager
 * type tag, BalanceManagerEvent queries), while CALLS target the upgraded
 * `DEEPBOOK_PACKAGE_ID`. */
export let DEEPBOOK_ORIGINAL_PACKAGE_ID: string | undefined;
export let DEEPBOOK_REGISTRY_ID: string | undefined;

// siws_session (session-tokens) deployment — wallet-rooted session login
// (Phantom / MetaMask). Both `undefined` where session login isn't deployed;
// the sign-in options simply don't render there.
export let SESSION_PACKAGE_ID: string | undefined;
export let SESSION_REGISTRY_ID: string | undefined;

// cctp_bridge package (cctp-contracts/) — the protocol's CCTP entry point.
// `undefined` until the package is published on this network; the Bridge
// page renders a "not deployed" state.
export let CCTP_BRIDGE_PACKAGE_ID: string | undefined;

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

/**
 * Resolve a supported-token catalog entry from a market symbol. Call sites pass
 * either the on-chain ticker (`TBTC`) or the stripped display alias (`BTC`, the
 * form `displayAsset()` produces), so match the ticker directly first, then the
 * ticker with a leading test-token `T` removed. Returns `null` for symbols that
 * aren't catalog tokens (e.g. native `SUI`).
 */
export function findToken(symbol: string | null | undefined): SupportedToken | null {
  if (!symbol) return null;
  const upper = symbol.trim().toUpperCase();
  if (!upper) return null;
  return (
    SUPPORTED_TOKENS.find((t) => t.ticker.toUpperCase() === upper) ??
    SUPPORTED_TOKENS.find((t) => t.ticker.toUpperCase().replace(/^T/, "") === upper) ??
    null
  );
}

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
  deepbook?: {
    packageId: string;
    originalPackageId: string;
    registryId: string;
    deepCoinType: string;
    poolCreationFee: string;
  } | null;
  sessionTokens?: {
    packageId: string;
    registryId: string;
  } | null;
  cctpBridge?: {
    packageId: string;
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

  const db = info.deepbook;
  DEEPBOOK_PACKAGE_ID = db?.packageId;
  DEEPBOOK_ORIGINAL_PACKAGE_ID = db?.originalPackageId;
  DEEPBOOK_REGISTRY_ID = db?.registryId;

  SESSION_PACKAGE_ID = info.sessionTokens?.packageId;
  SESSION_REGISTRY_ID = info.sessionTokens?.registryId;

  CCTP_BRIDGE_PACKAGE_ID = info.cctpBridge?.packageId;

  const tt = info.testTokens;
  TEST_TOKENS = tt
    ? Object.entries(tt.tokens).map(([symbol, t]) => ({
        symbol,
        coinType: t.coinType,
        faucetId: t.faucetId,
        decimals: t.decimals,
        module: t.coinType.split("::")[1] ?? symbol.toLowerCase(),
        // Derive the package from the token's own coinType, not the shared
        // testTokens.packageId — a token (e.g. prod TSUI) can live in a
        // different package than the rest, and the faucet must target the
        // package that actually contains its module.
        packageId: t.coinType.split("::")[0] ?? tt.packageId,
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
