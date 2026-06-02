// Single source of truth for which on-chain deployment the app targets.
//
// `VITE_ENVIRONMENT` (mainnet | testnet | devnet) selects an entry in
// rust-backend/deployments.json; the package / protocol-config / treasury
// ids are read from that entry. There are no per-id env vars — the deploy
// pipeline writes deployments.json and the frontend follows it.
//
// Ids are `string | undefined`: an environment with no published deployment
// (a `null` block, e.g. mainnet today) yields `undefined`, and the screens
// fall back to their existing "no deployment configured" / empty states
// instead of crashing.

import deployments from "../../rust-backend/deployments.json";

export type SuiEnvironment = "mainnet" | "testnet" | "devnet";

// Default to testnet so local dev runs with no env file.
export const ENV: SuiEnvironment =
  (import.meta.env.VITE_ENVIRONMENT as SuiEnvironment | undefined) ?? "testnet";

const info = deployments[ENV]?.package_info;

export const PACKAGE_ID: string | undefined = info?.packageId;
export const PROTOCOL_CONFIG_ID: string | undefined = info?.protocolConfigId;
export const TREASURY_ID: string | undefined = info?.treasuryId;

// Testnet faucet tokens (SO-93). Each is a shared `Faucet` with a public
// `mint_to_sender`. Only the testnet deployment publishes these; mainnet /
// devnet have a `null` package_info, so this resolves to `[]` and the
// faucet page falls back to a "testnet only" empty state.
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

const tt = info?.testTokens;
export const TEST_TOKENS: TestToken[] = tt
  ? Object.entries(tt.tokens).map(([symbol, t]) => ({
      symbol,
      coinType: t.coinType,
      faucetId: t.faucetId,
      decimals: t.decimals,
      module: t.coinType.split("::")[1] ?? symbol.toLowerCase(),
      packageId: tt.packageId,
    }))
  : [];
