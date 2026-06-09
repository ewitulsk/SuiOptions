// gas-station client. Two calls: fetch the sponsor wallet balance/health
// (used to default the toggle) and request a sponsored transaction.

import { GAS_STATION_URL } from "../config";

const base = () => GAS_STATION_URL.replace(/\/$/, "");

export type GasStationBalance = {
  address: string;
  balanceMist: string;
  balanceSui: number;
  thresholdMist: string;
  /** false when balance < threshold — the toggle defaults off. */
  healthy: boolean;
};

type BalanceDto = {
  address: string;
  balance_mist: string;
  balance_sui: number;
  threshold_mist: string;
  healthy: boolean;
};

export async function fetchGasStationBalance(): Promise<GasStationBalance> {
  const res = await fetch(`${base()}/balance`);
  if (!res.ok) throw new Error(`gas-station /balance → ${res.status}`);
  const d = (await res.json()) as BalanceDto;
  return {
    address: d.address,
    balanceMist: d.balance_mist,
    balanceSui: d.balance_sui,
    thresholdMist: d.threshold_mist,
    healthy: d.healthy,
  };
}

export type SponsorResult = {
  /** base64(bcs(TransactionData)) — the wallet co-signs these exact bytes. */
  txBytes: string;
  /** base64 sponsor signature over txBytes. */
  sponsorSignature: string;
  gasBudget: string;
  gasPrice: string;
};

type SponsorDto = {
  tx_bytes: string;
  sponsor_signature: string;
  gas_budget: string;
  gas_price: string;
};

/**
 * Ask the gas station to sponsor a gasless transaction. `kindBytesB64` is
 * base64 of the `TransactionKind` BCS (dapp-kit
 * `tx.build({ onlyTransactionKind: true })`).
 */
export async function requestSponsorship(
  sender: string,
  kindBytesB64: string,
): Promise<SponsorResult> {
  const res = await fetch(`${base()}/sponsor`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ sender, kind_bytes: kindBytesB64 }),
  });
  if (!res.ok) {
    const msg = await res.text().catch(() => "");
    throw new Error(`gas-station /sponsor → ${res.status}${msg ? `: ${msg}` : ""}`);
  }
  const d = (await res.json()) as SponsorDto;
  return {
    txBytes: d.tx_bytes,
    sponsorSignature: d.sponsor_signature,
    gasBudget: d.gas_budget,
    gasPrice: d.gas_price,
  };
}
