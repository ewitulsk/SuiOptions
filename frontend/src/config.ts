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
