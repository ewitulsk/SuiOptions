// The oracle switch, as the browser sees it (SO-335).
//
// `appraisal.ts` used to hardcode `oracle_pyth::attest` and hand-roll
// Pyth's four-call accumulator prefix, which made the oracle provider a
// property of the frontend BUILD. It isn't: it's a backend config field.
//
// oracle-service publishes the live provider and its on-chain adapter
// identity on `GET /oracle/descriptor`; this module fetches it. A
// provider switch is then a backend restart — the deployed frontend
// follows on its next descriptor read, with no rebuild and no redeploy.
//
// Keep in lockstep with `services/oracle-service/src/router.rs`
// (`OracleDescriptor`) and `crates/oracle-client` (its Rust twin).

import { useQuery } from "@tanstack/react-query";

import { ORACLE_SERVICE_URL } from "../config";

/** Providers the protocol has adapters for. Mirrors Rust's `OracleProvider`. */
export type OracleProvider = "pyth" | "switchboard";

/** On-chain identity of the LIVE provider's adapter. */
export type OracleAdapterIds = {
  /** Our adapter package (`oracle_pyth` / `oracle_switchboard`). */
  adapter_package_id: string;
  /** That adapter's shared feed registry. */
  feed_registry_id: string;
  /** `trading_vault::registry::OracleRegistry`. */
  oracle_registry_id: string;
};

export type OracleDescriptor = {
  provider: OracleProvider;
  /** Move module `attest` lives in for this provider. */
  adapter_module: string;
  /**
   * Absent when the live provider's adapter isn't deployed here. The
   * data plane still works; PTB composition does not — so callers must
   * treat this as "cannot build price legs", not as "use a default".
   */
  adapter?: OracleAdapterIds;
  /** canonical coin type → feed key under the live provider. */
  feeds: Record<string, string>;
};

export async function fetchOracleDescriptor(): Promise<OracleDescriptor> {
  const base = ORACLE_SERVICE_URL?.replace(/\/$/, "");
  if (!base) {
    throw new Error(
      "VITE_ORACLE_SERVICE_URL is not set — cannot resolve the live oracle provider",
    );
  }
  const res = await fetch(`${base}/oracle/descriptor`);
  if (!res.ok) {
    throw new Error(`oracle descriptor fetch failed: ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as OracleDescriptor;
}

/**
 * The live descriptor.
 *
 * Refetched on a slow interval rather than cached forever: a provider
 * switch happens while the tab is open, and a stale descriptor would
 * keep building the previous provider's legs against an adapter that may
 * since have been delisted — those PTBs abort on chain rather than
 * mispricing, but the user just sees a failed deposit.
 */
export function useOracleDescriptor() {
  return useQuery({
    queryKey: ["oracle-descriptor"],
    queryFn: fetchOracleDescriptor,
    staleTime: 60_000,
    refetchInterval: 5 * 60_000,
    retry: 2,
  });
}

/**
 * Guard for composing price legs. Throws with a reason rather than
 * returning a partial descriptor, because every caller needs the adapter
 * ids and a missing one is unrecoverable at the PTB level.
 */
export function requireAdapter(d: OracleDescriptor): OracleAdapterIds {
  if (!d.adapter) {
    throw new Error(
      `the live oracle provider (${d.provider}) has no adapter deployed on this network — ` +
        `deposits needing price attestations cannot be built`,
    );
  }
  return d.adapter;
}
