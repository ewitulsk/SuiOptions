// cctp-relay API client (see rust-backend/services/cctp-relay).

import { CCTP_URL } from "../config";

export type BridgeTransfer = {
  id: number;
  origin_chain: "sui" | "solana";
  origin_tx_hash: string;
  origin_wallet: string;
  destination_chain: "sui" | "solana";
  destination_wallet: string | null;
  mint_recipient: string | null;
  amount: number | null; // USDC base units (6 decimals)
  status: "pending_attestation" | "attested" | "minting" | "complete" | "failed";
  mint_tx_hash: string | null;
  error: string | null;
  burned_at_ms: number | null;
  attested_at_ms: number | null;
  minted_at_ms: number | null;
  /** End-to-end bridge time (source burn → destination mint); null in flight. */
  duration_ms: number | null;
  created_at_ms: number;
};

const base = () => CCTP_URL.replace(/\/$/, "");

export async function registerBridgeTransfer(params: {
  txHash: string;
  originChain: "sui" | "solana";
  wallet: string;
  destinationWallet?: string;
}): Promise<BridgeTransfer> {
  const res = await fetch(`${base()}/transfers`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      tx_hash: params.txHash,
      origin_chain: params.originChain,
      wallet: params.wallet,
      destination_wallet: params.destinationWallet,
    }),
  });
  if (!res.ok) {
    throw new Error(`cctp-relay POST /transfers → ${res.status}: ${await res.text()}`);
  }
  return (await res.json()) as BridgeTransfer;
}

export async function fetchBridgeTransfers(wallet: string): Promise<BridgeTransfer[]> {
  const res = await fetch(`${base()}/transfers?wallet=${encodeURIComponent(wallet)}`);
  if (!res.ok) {
    throw new Error(`cctp-relay GET /transfers → ${res.status}`);
  }
  return (await res.json()) as BridgeTransfer[];
}
