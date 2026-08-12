import type { Network } from "@yourorg/sui-siws-session";

// Fill these from `sui client publish` output (see ../contracts/README.md),
// either here or via a `.env` file (VITE_PACKAGE_ID / VITE_REGISTRY_ID / ...).
const env = import.meta.env;

export const NETWORK: Network = (env.VITE_NETWORK as Network) ?? "testnet";
export const PACKAGE_ID: string = env.VITE_PACKAGE_ID ?? "";
export const REGISTRY_ID: string = env.VITE_REGISTRY_ID ?? "";
export const COIN_TYPE: string = env.VITE_COIN_TYPE ?? "0x2::sui::SUI";

/** The Move allowlist literal (symbolic package name, see contracts). This is
 *  the on-chain SessionCap allowlist — it gates which app functions a cap may
 *  call and uses the symbolic package name baked into the Move const. */
export const WITHDRAW_SELECTOR = "siws_session::app_example::withdraw";

/**
 * The sponsor's defense-in-depth allowlist: every `pkg::module::function` the
 * sponsor will pay gas + co-sign for. This MUST include the session entrypoints
 * (sign-in, revoke) — not just the app call — because the sponsor co-signs all
 * three. Uses the deployed numeric package id.
 */
export const SPONSORED_TARGETS = [
  `${PACKAGE_ID}::session::verify_and_open_session`,
  `${PACKAGE_ID}::session::verify_and_open_session_eth`,
  `${PACKAGE_ID}::session::revoke_all`,
  `${PACKAGE_ID}::session::revoke_all_eth`,
  `${PACKAGE_ID}::app_example::withdraw`,
];

export const CONFIGURED = PACKAGE_ID !== "" && REGISTRY_ID !== "";

// Conservative demo caps.
export const SPEND_CAP = 1_000_000_000n; // 1 SUI total
export const PER_TX_CAP = 100_000_000n; //  0.1 SUI per tx
export const TTL_MS = 15 * 60_000; //       15 minutes
