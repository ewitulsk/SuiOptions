// `GET /oracle/legs` — the live provider's signed off-chain payload for
// one PTB's price legs (SO-346, browser reader added in SO-356).
//
// The response is provider-TAGGED: the composer dispatches on what
// oracle-service says, never on anything compiled into this bundle. The
// server owns the hard parts — Switchboard signer draws are retried
// against attested signers and recovery bytes are corrected before the
// signatures reach us; Pyth accumulators come from its authenticated
// Hermes client. This module just parses wire encodings (hex/base64
// bytes, decimal-string u128s — they exceed 2^53, hence BigInt).

import { fromBase64, fromHex } from "@mysten/sui/utils";
import { ORACLE_SERVICE_URL } from "../config";

/** One signed Switchboard consensus payload, submit-ready. */
export type SwitchboardQuote = {
  /** 32-byte feed hashes, parallel with values/valuesNeg/minOracleSamples. */
  feedIds: Uint8Array[];
  /** 18-decimal fixed-point magnitudes. */
  values: bigint[];
  valuesNeg: boolean[];
  minOracleSamples: number[];
  /** Per-oracle signatures (r‖s‖v, recovery byte pre-corrected server-side). */
  signatures: Uint8Array[];
  slot: bigint;
  timestampSeconds: bigint;
  /** Sui `Oracle` object ids, in signature order. */
  oracleIds: string[];
};

export type OracleLegsPayload =
  | {
      provider: "pyth";
      accumulatorUpdate: Uint8Array;
      /** canonical coin type → feed id covered by the update. */
      feeds: Record<string, string>;
    }
  | {
      provider: "switchboard";
      /** Switchboard's own `on_demand` package (exposes `run_N`). */
      switchboardPackageId: string;
      /** The `Queue` object `run_N` validates signers against. */
      queueId: string;
      /** canonical coin type → feed hash covered by `quote`. */
      feedHashes: Record<string, string>;
      quote: SwitchboardQuote;
    };

type WirePyth = {
  provider: "pyth";
  accumulator_update_b64: string;
  feeds: Record<string, string>;
};

type WireSwitchboard = {
  provider: "switchboard";
  switchboard_package_id: string;
  queue_id: string;
  feed_hashes: Record<string, string>;
  quote: {
    feed_ids: string[];
    values: string[];
    values_neg: boolean[];
    min_oracle_samples: number[];
    signatures_b64: string[];
    slot: number;
    timestamp_seconds: number;
    oracle_ids: string[];
  };
};

export async function fetchOracleLegs(assets: string[]): Promise<OracleLegsPayload> {
  if (assets.length === 0) throw new Error("no assets for oracle legs");
  const base = ORACLE_SERVICE_URL?.replace(/\/$/, "");
  if (!base) {
    throw new Error("VITE_ORACLE_SERVICE_URL is not set — cannot fetch oracle legs");
  }
  const qs = encodeURIComponent(assets.join(","));
  // Base-relative PUBLIC shape (SO-359) — see oracleDescriptor.ts.
  const res = await fetch(`${base}/legs?assets=${qs}`);
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(
      `oracle legs fetch failed: ${res.status} ${res.statusText}${body ? ` — ${body}` : ""}`,
    );
  }
  const wire = (await res.json()) as WirePyth | WireSwitchboard;
  switch (wire.provider) {
    case "pyth":
      return {
        provider: "pyth",
        accumulatorUpdate: fromBase64(wire.accumulator_update_b64),
        feeds: wire.feeds,
      };
    case "switchboard": {
      const q = wire.quote;
      const feeds = q.feed_ids.length;
      if (q.values.length !== feeds || q.values_neg.length !== feeds || q.min_oracle_samples.length !== feeds) {
        throw new Error("switchboard quote arrays are not parallel");
      }
      if (q.signatures_b64.length !== q.oracle_ids.length) {
        throw new Error("switchboard signatures/oracles are not parallel");
      }
      return {
        provider: "switchboard",
        switchboardPackageId: wire.switchboard_package_id,
        queueId: wire.queue_id,
        feedHashes: wire.feed_hashes,
        quote: {
          feedIds: q.feed_ids.map(fromHex),
          values: q.values.map((v) => BigInt(v)),
          valuesNeg: q.values_neg,
          minOracleSamples: q.min_oracle_samples,
          signatures: q.signatures_b64.map(fromBase64),
          slot: BigInt(q.slot),
          timestampSeconds: BigInt(q.timestamp_seconds),
          oracleIds: q.oracle_ids,
        },
      };
    }
    default:
      throw new Error(
        `oracle legs response has unknown provider ${(wire as { provider?: string }).provider}`,
      );
  }
}
