// cctp-relay config client (see rust-backend/services/cctp-relay/src/router.rs).
//
// The CCTP constants live in the service, not here. They are NOT derivable
// from `VITE_ENVIRONMENT`: the bridge runs on its own network, independent of
// the protocol's — staging, for instance, has the protocol on testnet and the
// bridge on mainnet. Deriving them from `ENV` is what previously paired a
// mainnet bridge with testnet Circle ids.

import { CCTP_URL } from "../config";

export type CctpConfig = {
  domainSui: number;
  domainSolana: number;
  sui: {
    /** Network the burn PTB must be signed against (`testnet` | `mainnet`). */
    network: string;
    messageTransmitterPackage: string;
    tokenMessengerPackage: string;
    messageTransmitterState: string;
    tokenMessengerState: string;
    usdcTreasury: string;
    usdcCoinType: string;
  };
  solana: {
    network: string;
    rpcUrl: string;
    usdcMint: string;
    tokenMessengerProgram: string;
    messageTransmitterProgram: string;
  };
};

export async function fetchCctpConfig(): Promise<CctpConfig> {
  const res = await fetch(`${CCTP_URL.replace(/\/$/, "")}/config`);
  if (!res.ok) {
    throw new Error(`cctp-relay GET /config → ${res.status}`);
  }
  return (await res.json()) as CctpConfig;
}
